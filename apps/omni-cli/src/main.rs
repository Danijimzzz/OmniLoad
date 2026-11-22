//! `omni`: línea de comandos de OmniLoad.
//!
//! ```text
//! omni get <url> [-o archivo | -d carpeta] [-x conexiones] [--sha256 HEX]
//! omni add <url>... [-d carpeta] [--paquete NOMBRE] [--parada]
//! omni list
//! omni pause [id...] | omni resume [id...] | omni remove <id...>
//! omni limit <8M | off> [--id ID...]
//! omni check <url o archivo>...
//! omni grab <url o archivo>...
//! omni service status | stop
//! ```
//!
//! `get` descarga directamente en la terminal (sin servicio): Ctrl+C pausa la
//! descarga guardando el progreso y ejecutar el mismo comando la reanuda.
//! `check` comprueba enlaces sin descargar nada (también sin servicio).
//!
//! El resto habla con el servicio `omnid` (el mismo que usa la ventana), y lo
//! lanza si no está en marcha: las descargas siguen aunque se cierre la
//! terminal.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use omni_api::{Client, Paths, connect_or_launch, omnid_exe};
use omni_core::{Destination, Download, DownloadOptions, Error};
use omni_grab::{Check, Grabber};
use omni_hosts::Hosts;
use omni_queue::Resolver;
use omni_queue::{JobInfo, NewJob};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "omni", version, about = "OmniLoad: gestor de descargas")]
struct Cli {
    /// Mostrar más detalle (reintentos, reanudación).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Descarga un archivo en varias partes, reanudando si ya estaba a medias.
    Get(GetArgs),
    /// Añade enlaces a la cola del servicio (la misma que ve la ventana).
    Add(AddArgs),
    /// Muestra la lista de descargas.
    #[command(alias = "ls")]
    List,
    /// Pausa descargas (sin ids: todas).
    Pause { ids: Vec<u64> },
    /// Reanuda descargas (sin ids: todas).
    Resume { ids: Vec<u64> },
    /// Quita descargas de la lista (no borra archivos salvo con --archivos).
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        ids: Vec<u64>,
        /// Borra también los archivos descargados y la carpeta si queda vacía.
        #[arg(long)]
        archivos: bool,
    },
    /// Límite de velocidad: global, o de unas descargas con --id.
    Limit(LimitArgs),
    /// Redes (interfaces) por las que se descarga a la vez. Sin argumentos:
    /// lista las que hay en este equipo y las que están en uso.
    #[command(name = "redes")]
    Networks {
        /// Nombres de interfaz a usar a la vez (mira la lista).
        names: Vec<String>,
        /// Volver a la red del sistema.
        #[arg(long)]
        sistema: bool,
    },
    /// Comprueba enlaces sin descargarlos: nombre, tamaño y si están
    /// disponibles. Acepta también archivos .txt o .html con enlaces.
    Check {
        #[arg(required = true, value_name = "URL_O_ARCHIVO")]
        links: Vec<String>,
    },
    /// Manda enlaces al capturador del servicio (aparecen en la ventana para
    /// elegir qué descargar).
    Grab {
        #[arg(required = true, value_name = "URL_O_ARCHIVO")]
        links: Vec<String>,
    },
    /// Diagnóstico de BitTorrent: añade un magnet o un .torrent y cuenta
    /// durante unos segundos pares, velocidad y errores. Baja datos de verdad
    /// (un trozo del primer archivo) a una carpeta temporal que se borra.
    TorrentDebug {
        #[arg(value_name = "MAGNET_O_ARCHIVO_TORRENT")]
        source: String,
        /// Cuánto tiempo mirar, en segundos.
        #[arg(long, default_value_t = 60)]
        segundos: u64,
    },
    /// El servicio en segundo plano.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Args)]
struct AddArgs {
    /// Uno o varios enlaces.
    #[arg(required = true)]
    urls: Vec<String>,
    /// Carpeta de destino.
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,
    /// Nombre del paquete.
    #[arg(short, long, default_value = "Desde la terminal")]
    paquete: String,
    /// Añadir sin empezar.
    #[arg(long)]
    parada: bool,
}

#[derive(Args)]
struct LimitArgs {
    /// Velocidad por segundo (por ejemplo 500K, 2M) u "off" para quitarlo.
    speed: String,
    /// Solo estas descargas.
    #[arg(long = "id")]
    ids: Vec<u64>,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// ¿Está en marcha?
    Status,
    /// Pararlo (guarda el progreso; lo que se descargaba sigue al volver).
    Stop,
}

#[derive(Args)]
struct GetArgs {
    /// URL http o https.
    url: String,
    /// Ruta exacta del archivo de destino.
    #[arg(short, long, conflicts_with = "dir")]
    output: Option<PathBuf>,
    /// Carpeta de destino; el nombre sale del servidor o de la URL.
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,
    /// Conexiones simultáneas.
    #[arg(short = 'x', long, default_value_t = omni_core::DEFAULT_CONNECTIONS_PER_NETWORK)]
    connections: usize,
    /// Tamaño de bloque (por ejemplo 8M o 512K).
    #[arg(long, default_value = "8M", value_parser = parse_size)]
    block_size: u64,
    /// SHA-256 esperado; se comprueba al terminar.
    #[arg(long)]
    sha256: Option<String>,
    /// Sobrescribir el destino si ya existe.
    #[arg(short, long)]
    force: bool,
    /// No mostrar la barra de progreso.
    #[arg(short, long)]
    quiet: bool,
}

/// "8M", "512K", "1G" o bytes a secas.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| format!("tamaño no válido: {s}"))?;
    match n.checked_mul(mult) {
        Some(0) | None => Err(format!("tamaño no válido: {s}")),
        Some(v) => Ok(v),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: no se pudo iniciar tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: bool) {
    let default = if verbose {
        "info,omni_core=debug"
    } else {
        "warn,librqbit=info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Get(args) => get(args).await,
        Command::Service { action } => service(action).await,
        Command::Check { links } => check(&links).await,
        Command::TorrentDebug { source, segundos } => torrent_debug(&source, segundos).await,
        Command::Networks { names, sistema } if names.is_empty() && !sistema => {
            list_networks().await;
            Ok(ExitCode::SUCCESS)
        }
        cmd => {
            let c = connect().await?;
            remote(&c, cmd).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn paths() -> Result<Paths> {
    Paths::system().context("no se encontró la carpeta de datos del sistema (usa OMNILOAD_DB)")
}

/// Se conecta al servicio, lanzándolo si hace falta.
async fn connect() -> Result<Client> {
    let paths = paths()?;
    let exe = omnid_exe();
    connect_or_launch(&paths, exe.as_deref(), Duration::from_secs(10))
        .await
        .map_err(|e| match exe {
            Some(_) => anyhow::anyhow!(e),
            None => anyhow::anyhow!(
                "{e}. No encuentro el programa omnid junto a omni; compílalo con \
                 `cargo build -p omnid`"
            ),
        })
}

async fn service(action: ServiceAction) -> Result<ExitCode> {
    let paths = paths()?;
    let client = Client::connect(&paths).await;
    match (action, client) {
        (ServiceAction::Status, Some(c)) => {
            let st = c.status().await?;
            println!("omnid {} en marcha (pid {})", st.version, st.pid);
            println!("Base de datos: {}", paths.db.display());
        }
        (ServiceAction::Status, None) => println!("omnid no está en marcha"),
        (ServiceAction::Stop, Some(c)) => {
            c.shutdown().await?;
            println!("Parando omnid; el progreso queda guardado.");
        }
        (ServiceAction::Stop, None) => println!("omnid no estaba en marcha"),
    }
    Ok(ExitCode::SUCCESS)
}

async fn remote(c: &Client, cmd: Command) -> Result<()> {
    match cmd {
        Command::Add(a) => {
            let dir = std::path::absolute(&a.dir).context("carpeta no válida")?;
            for url in a.urls {
                let new = NewJob {
                    package: a.paquete.clone(),
                    start: !a.parada,
                    ..NewJob::new(url.clone(), dir.clone())
                };
                match c.add(&new).await {
                    Ok(id) => println!("{id}\t{url}"),
                    Err(e) => eprintln!("error: {url}: {e}"),
                }
            }
        }
        Command::List => print_jobs(&c.jobs().await?),
        Command::Pause { ids } if ids.is_empty() => c.pause_all().await?,
        Command::Pause { ids } => c.stop(&ids).await?,
        Command::Resume { ids } if ids.is_empty() => c.start_all().await?,
        Command::Resume { ids } => c.start(&ids).await?,
        Command::Remove { ids, archivos } => c.remove_with(&ids, archivos, None).await?,
        Command::Limit(l) => {
            let speed = match l.speed.as_str() {
                "off" | "no" | "0" => None,
                s => Some(parse_size(s).map_err(anyhow::Error::msg)?),
            };
            if l.ids.is_empty() {
                let mut s = c.settings().await?;
                s.speed_limit = speed;
                c.set_settings(&s).await?;
            } else {
                c.set_job_speed_limit(&l.ids, speed).await?;
            }
        }
        Command::Grab { links } => {
            let req = omni_api::AddLinksRequest {
                text: links_text(&links)?,
                ..Default::default()
            };
            let added = c.add_links(&req).await?;
            println!(
                "{} enlaces al capturador: ábrelo en la ventana para elegir qué descargar",
                added.added
            );
        }
        Command::Networks { names, .. } => {
            let found = omni_core::interfaces();
            if let Some(bad) = names.iter().find(|n| !found.iter().any(|i| &i.name == *n)) {
                anyhow::bail!("no hay ninguna interfaz «{bad}»; mira `omni redes`");
            }
            let mut s = c.settings().await?;
            s.networks = names;
            c.set_settings(&s).await?;
        }
        Command::Get(_)
        | Command::Service { .. }
        | Command::Check { .. }
        | Command::TorrentDebug { .. } => unreachable!(),
    }
    Ok(())
}

/// Las interfaces de este equipo y cuáles usa el servicio.
async fn list_networks() {
    let in_use: Vec<String> = match connect().await {
        Ok(c) => c.settings().await.map(|s| s.networks).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let found = omni_core::interfaces();
    if found.is_empty() {
        println!("No se encontró ninguna interfaz de red con dirección.");
        return;
    }
    for i in found {
        let mark = if in_use.contains(&i.name) { "*" } else { " " };
        println!("{mark} {:<24} {}", i.name, i.ip);
    }
    if in_use.is_empty() {
        println!("(sin marcas: se descarga por la red del sistema)");
    } else {
        println!("(* = en uso)");
    }
}

/// Los enlaces de los argumentos: URLs sueltas o archivos con enlaces.
fn links_text(args: &[String]) -> Result<String> {
    let mut out = String::new();
    for a in args {
        let path = std::path::Path::new(a);
        if path.is_file()
            && let Some(url) = omni_grab::torrent_file_url(path)
        {
            out.push_str(url.as_str());
            out.push('\n');
        } else if path.is_file() {
            let bytes = std::fs::read(path).with_context(|| format!("no se pudo leer {a}"))?;
            let urls = omni_grab::links_from_file(a, &bytes).map_err(anyhow::Error::msg)?;
            for u in urls {
                out.push_str(u.as_str());
                out.push('\n');
            }
        } else {
            out.push_str(a);
            out.push('\n');
        }
    }
    Ok(out)
}

/// `omni check`: comprueba enlaces aquí mismo, sin servicio.
async fn torrent_debug(source: &str, seconds: u64) -> Result<ExitCode> {
    let text = links_text(&[source.to_owned()])?;
    let url = omni_grab::extract_urls(&text)
        .into_iter()
        .next()
        .context("no es un magnet ni un archivo .torrent")?;
    let net = omni_core::Network::system_default()?;
    println!("enlace: {url}");
    omni_hosts::Torrents::new()
        .debug(net.client(), &url, seconds, |l| println!("{l}"))
        .await?;
    Ok(ExitCode::SUCCESS)
}

async fn check(links: &[String]) -> Result<ExitCode> {
    let text = links_text(links)?;
    let net = omni_core::Network::system_default()?;
    let g = Grabber::new(
        tokio::runtime::Handle::current(),
        net.client().clone(),
        Hosts::standard().checkers(),
    );
    g.add_to_batch(&text, 0).map_err(anyhow::Error::msg)?;
    loop {
        let pending = g
            .snapshot()
            .iter()
            .flat_map(|p| &p.items)
            .any(|i| i.check == Check::Checking);
        if !pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for note in g.take_notes() {
        eprintln!("{note}");
    }
    let mut offline = 0;
    for p in g.snapshot() {
        println!("{}", p.name);
        for i in &p.items {
            let size = i
                .size
                .map(|s| HumanBytes(s).to_string())
                .unwrap_or_else(|| "?".into());
            let state = match &i.check {
                Check::Online { resumable: true } => "disponible".to_owned(),
                Check::Online { resumable: false } => "disponible (una conexión)".to_owned(),
                Check::Offline(e) => {
                    offline += 1;
                    format!("no disponible: {e}")
                }
                Check::Checking => unreachable!(),
            };
            println!("  {}\t{size}\t{state}\t{}", i.name, i.url);
        }
    }
    Ok(if offline > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_jobs(jobs: &[JobInfo]) {
    if jobs.is_empty() {
        println!("No hay descargas.");
        return;
    }
    println!(
        "{:>4}  {:<14} {:>6}  {:>10}  {:>11}  NOMBRE",
        "ID", "ESTADO", "%", "TAMAÑO", "VELOCIDAD"
    );
    for j in jobs {
        let pct = j
            .fraction()
            .map(|f| format!("{:.1}", f * 100.0))
            .unwrap_or_default();
        let size = j
            .total
            .map(|t| HumanBytes(t).to_string())
            .unwrap_or_default();
        let rate = if j.rate > 0.0 {
            format!("{}/s", HumanBytes(j.rate as u64))
        } else {
            String::new()
        };
        let status: String = j.status().chars().take(14).collect();
        println!(
            "{:>4}  {:<14} {:>6}  {:>10}  {:>11}  {}",
            j.id, status, pct, size, rate, j.name
        );
    }
}

async fn get(args: GetArgs) -> Result<ExitCode> {
    if args.connections == 0 {
        bail!("hace falta al menos una conexión");
    }
    let dest = match args.output {
        Some(p) => Destination::File(p),
        None => Destination::Dir(args.dir),
    };
    let opts = DownloadOptions {
        connections_per_network: args.connections,
        block_size: args.block_size,
        expected_sha256: args.sha256,
        overwrite: args.force,
        ..DownloadOptions::default()
    };
    let cancel = CancellationToken::new();
    // Pixeldrain, Mediafire…: el enlace directo se pide al plugin del host.
    let net = omni_core::Network::system_default()?;
    let link = Hosts::standard()
        .resolver(net.client().clone())
        .resolve(&args.url, &cancel)
        .await
        .context("no se pudo resolver el enlace")?;
    let mut opts = opts;
    opts.headers.extend(link.headers);
    let download =
        Download::new(&link.url, dest, opts).context("no se pudo preparar la descarga")?;
    let progress = download.progress();

    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }

    let bar = if args.quiet {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(0)
    };
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} · quedan {eta} · {msg}",
        )
        .expect("plantilla válida")
        .progress_chars("=> "),
    );
    let ticker = {
        let connections = args.connections;
        let bar = bar.clone();
        let progress = progress.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(200));
            loop {
                iv.tick().await;
                if let Some(total) = progress.total() {
                    bar.set_length(total);
                }
                bar.set_position(progress.downloaded());
                let stats = progress.per_network();
                let receiving: usize = stats.values().map(|s| s.receiving).sum();
                let retries: u64 = stats.values().map(|s| s.retries).sum();
                let limit: usize = stats.values().map(|s| s.limit).sum();
                let mut msg = format!("{receiving}/{connections} conexiones activas");
                if limit > 0 && limit < connections {
                    msg.push_str(&format!(" (el servidor solo acepta {limit})"));
                }
                if retries > 0 {
                    msg.push_str(&format!(" · {retries} reintentos"));
                }
                bar.set_message(msg);
            }
        })
    };

    let result = download.run(cancel).await;
    ticker.abort();
    bar.finish_and_clear();

    match result {
        Ok(out) => {
            if !args.quiet {
                let resumed = if out.resumed_from > 0 {
                    format!(" (reanudada desde {})", HumanBytes(out.resumed_from))
                } else {
                    String::new()
                };
                eprintln!(
                    "Descargado {} ({}){resumed}",
                    out.path.display(),
                    HumanBytes(out.size)
                );
                if !out.segmented {
                    eprintln!("Aviso: el servidor no acepta rangos; se usó una sola conexión.");
                }
                if let Some(h) = out.sha256 {
                    eprintln!("SHA-256 correcto: {h}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(Error::Cancelled) => {
            eprintln!(
                "Descarga pausada ({} guardados). Ejecuta el mismo comando para reanudarla.",
                HumanBytes(progress.downloaded())
            );
            Ok(ExitCode::from(130))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8M"), Ok(8 * 1024 * 1024));
        assert_eq!(parse_size("512k"), Ok(512 * 1024));
        assert_eq!(parse_size("100"), Ok(100));
        assert!(parse_size("0").is_err());
        assert!(parse_size("abc").is_err());
    }
}
