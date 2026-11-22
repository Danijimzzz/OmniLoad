//! Aviso de versión nueva y actualización dentro de la app.
//!
//! Al abrir (si no se desactiva) pregunta a GitHub cuál es la última versión
//! publicada y, si es más nueva que esta, avisa. Solo se envía una petición
//! `GET` a la API pública de GitHub, con el nombre y número de versión del
//! programa en la cabecera `User-Agent` (lo exige GitHub) y nada más: ni
//! identificador, ni datos del equipo.
//!
//! **Nada se descarga ni se instala sin que el usuario pulse «Actualizar
//! ahora».** Entonces se baja el instalador de esa versión (Windows: el
//! `.exe`; macOS: el `.dmg`; Linux: el `.AppImage` si se ejecuta como tal),
//! se comprueba su SHA-256 contra `SHA256SUMS.txt` de la misma versión y,
//! al cerrarse la app, se instala y se vuelve a abrir. Si no hay instalador
//! para este sistema, solo se abre la página de la versión.

use std::cmp::Ordering;
use std::time::Duration;

use serde::Deserialize;

/// La versión de este programa.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// La parte numérica de la versión, sin el sufijo `-prN` (`0.2.0`).
pub fn base_version(version: &str) -> &str {
    version.split(['-', '+']).next().unwrap_or(version)
}

/// Número del PR de una versión de PR sin aprobar (`0.2.0-pr9` → `Some(9)`).
/// Al aprobarse el PR se quita el sufijo y vuelve a ser una versión normal.
pub fn pr_number(version: &str) -> Option<u32> {
    version.split_once('-')?.1.strip_prefix("pr")?.parse().ok()
}

/// El instalador de una versión para este sistema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Dirección de `SHA256SUMS.txt` de la misma versión.
    pub sums_url: String,
}

/// Qué se encontró.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Hay una versión más nueva, y quizá un instalador para este sistema.
    Newer {
        version: String,
        url: String,
        install: Option<Install>,
    },
    /// Esta es la última.
    UpToDate { latest: String },
    /// Todavía no hay ninguna versión publicada en GitHub.
    NoReleases,
    /// No se pudo comprobar (sin internet, GitHub no contesta…).
    Failed(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Dirección de la API de la última versión del repositorio.
fn api_url() -> String {
    let repo = env!("CARGO_PKG_REPOSITORY").trim_end_matches('/');
    let path = repo.strip_prefix("https://github.com/").unwrap_or(repo);
    format!("https://api.github.com/repos/{path}/releases/latest")
}

/// Página de versiones del repositorio.
pub fn releases_page() -> String {
    format!(
        "{}/releases",
        env!("CARGO_PKG_REPOSITORY").trim_end_matches('/')
    )
}

/// Pregunta a GitHub por la última versión.
pub async fn check() -> Outcome {
    let client = match reqwest::Client::builder()
        .user_agent(format!("OmniLoad/{CURRENT}"))
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let resp = match client
        .get(api_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(short_error(&e)),
    };
    let status = resp.status().as_u16();
    match resp.text().await {
        Ok(body) => outcome(status, &body, CURRENT),
        Err(e) => Outcome::Failed(short_error(&e)),
    }
}

fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "GitHub tardó demasiado en contestar".into()
    } else if e.is_connect() {
        "no hay conexión con GitHub".into()
    } else {
        "no se pudo hablar con GitHub".into()
    }
}

/// Interpreta la respuesta de GitHub (separado para poder probarlo).
pub fn outcome(status: u16, body: &str, current: &str) -> Outcome {
    match status {
        404 => return Outcome::NoReleases,
        200 => {}
        403 | 429 => {
            return Outcome::Failed("GitHub pide esperar un rato; prueba más tarde".into());
        }
        s => return Outcome::Failed(format!("GitHub contestó con el código {s}")),
    }
    let Ok(rel) = serde_json::from_str::<Release>(body) else {
        return Outcome::Failed("respuesta de GitHub ilegible".into());
    };
    if rel.draft || rel.prerelease {
        return Outcome::NoReleases;
    }
    let latest = rel.tag_name.trim_start_matches(['v', 'V']).to_owned();
    let install = pick_install(&rel.assets, platform());
    match compare(&latest, current) {
        Some(Ordering::Greater) if rel.html_url.starts_with("https://github.com/") => {
            Outcome::Newer {
                version: latest,
                url: rel.html_url,
                install,
            }
        }
        Some(Ordering::Greater) => Outcome::Newer {
            version: latest,
            url: releases_page(),
            install,
        },
        Some(_) => Outcome::UpToDate { latest },
        None => Outcome::Failed(format!("versión «{}» no reconocida", rel.tag_name)),
    }
}

/// Qué instalador sirve a este sistema (`None` si ninguno).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    WindowsX64,
    MacArm64,
    MacX64,
    /// Linux ejecutado como AppImage: se puede reemplazar a sí mismo.
    LinuxAppImage,
    Other,
}

pub fn platform() -> Platform {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Platform::WindowsX64
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Platform::MacArm64
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Platform::MacX64
    } else if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_some() {
        Platform::LinuxAppImage
    } else {
        Platform::Other
    }
}

/// Elige el instalador de `platform` entre los archivos de una versión. Solo
/// vale si la versión trae `SHA256SUMS.txt` y todo viene de GitHub.
fn pick_install(assets: &[Asset], platform: Platform) -> Option<Install> {
    let suffix = match platform {
        Platform::WindowsX64 => "-windows-x64.exe",
        Platform::MacArm64 => "-macos-arm64.dmg",
        Platform::MacX64 => "-macos-x64.dmg",
        Platform::LinuxAppImage => "-linux-x64.AppImage",
        Platform::Other => return None,
    };
    let trusted = |a: &&Asset| a.browser_download_url.starts_with("https://github.com/");
    let file = assets
        .iter()
        .filter(trusted)
        .find(|a| a.name.starts_with("OmniLoad-") && a.name.ends_with(suffix))?;
    // Sin portable ni zip: «Setup» solo en el de Windows.
    if platform == Platform::WindowsX64 && !file.name.starts_with("OmniLoad-Setup-") {
        return None;
    }
    let sums = assets
        .iter()
        .filter(trusted)
        .find(|a| a.name == "SHA256SUMS.txt")?;
    Some(Install {
        name: file.name.clone(),
        url: file.browser_download_url.clone(),
        size: file.size,
        sums_url: sums.browser_download_url.clone(),
    })
}

/// La suma de `name` en un `SHA256SUMS.txt` (`<hex>  <nombre>` por línea).
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name
            && hash.len() == 64
            && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
    })
}

/// Baja el instalador a una carpeta temporal y comprueba su SHA-256.
/// `progress` recibe (bajado, total) mientras baja.
pub async fn download(
    install: &Install,
    progress: impl Fn(u64, u64),
) -> Result<std::path::PathBuf, String> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let client = reqwest::Client::builder()
        .user_agent(format!("OmniLoad/{CURRENT}"))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let sums = client
        .get(&install.sums_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("no se pudo leer SHA256SUMS.txt: {}", short_error(&e)))?
        .text()
        .await
        .map_err(|e| short_error(&e))?;
    let expected = sum_for(&sums, &install.name)
        .ok_or("la versión no trae la suma de comprobación del instalador")?;

    let dir = std::env::temp_dir().join(format!("omniload-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&install.name);
    let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut resp = client
        .get(&install.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("no se pudo descargar: {}", short_error(&e)))?;
    let total = resp.content_length().unwrap_or(install.size);
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(|e| short_error(&e))? {
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    let actual = hex(&hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        return Err("la descarga no coincide con su suma de comprobación; no se instala".into());
    }
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Instala lo descargado y vuelve a abrir OmniLoad. Se llama justo antes de
/// salir, con el servicio ya parado. El instalador (o un pequeño script en
/// macOS y Linux) sigue vivo cuando esta app se cierra.
pub fn launch_installer(file: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        std::process::Command::new(file)
            .args([
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
            ])
            .creation_flags(0x0000_0008 | 0x0000_0200)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("no se pudo lanzar el instalador: {e}"))
    }
    #[cfg(not(windows))]
    {
        let me = std::process::id();
        let script = unix_update_script(file, me)?;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("no se pudo lanzar la actualización: {e}"))
    }
}

/// El script que espera a que esta app salga, reemplaza el programa y lo
/// abre (macOS y Linux).
#[cfg(not(windows))]
fn unix_update_script(file: &std::path::Path, pid: u32) -> Result<String, String> {
    let q = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let wait = format!("while kill -0 {pid} 2>/dev/null; do sleep 0.3; done\n");
    if cfg!(target_os = "macos") {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let app = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or("OmniLoad no está en una carpeta .app: abre el .dmg a mano")?
            .to_path_buf();
        Ok(format!(
            "{wait}MNT=$(mktemp -d)\n\
             if hdiutil attach -nobrowse -readonly -mountpoint \"$MNT\" {dmg} >/dev/null; then\n\
               if rm -rf {app} && cp -R \"$MNT/OmniLoad.app\" {app}; then\n\
                 hdiutil detach \"$MNT\" >/dev/null; xattr -dr com.apple.quarantine {app} 2>/dev/null; open {app}; exit 0\n\
               fi\n\
               hdiutil detach \"$MNT\" >/dev/null\n\
             fi\n\
             open {dmg}\n",
            dmg = q(file),
            app = q(&app),
        ))
    } else {
        let target = std::env::var_os("APPIMAGE")
            .map(std::path::PathBuf::from)
            .ok_or("no se ejecuta como AppImage")?;
        Ok(format!(
            "{wait}chmod +x {new} && mv -f {new} {target} && ( {target} >/dev/null 2>&1 & )\n",
            new = q(file),
            target = q(&target),
        ))
    }
}

/// Compara dos versiones `1.2.3` (con `v` delante o `-beta` detrás). Una
/// versión con sufijo (`1.0.0-beta`) es anterior a la misma sin él.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(parse(a)?.cmp(&parse(b)?))
}

fn parse(v: &str) -> Option<([u64; 3], bool)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut parts = [0u64; 3];
    let mut n = 0;
    for p in core.split('.') {
        *parts.get_mut(n)? = p.parse().ok()?;
        n += 1;
    }
    // El segundo valor es «versión final»: false (con sufijo) < true.
    (n > 0).then_some((parts, !pre))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, extra: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/Danijimzzz/OmniLoad/releases/tag/{tag}"{extra}}}"#
        )
    }

    #[test]
    fn pr_versions_are_recognised() {
        assert_eq!(pr_number("0.2.0-pr9"), Some(9));
        assert_eq!(pr_number("0.2.0-pr12"), Some(12));
        assert_eq!(pr_number("0.2.0"), None);
        assert_eq!(pr_number("0.2.0-beta"), None);
        assert_eq!(pr_number("0.2.0-prx"), None);
        assert_eq!(base_version("0.2.0-pr9"), "0.2.0");
        assert_eq!(base_version("0.2.0"), "0.2.0");
        // Una versión de PR es anterior a la misma ya aprobada.
        assert_eq!(compare("0.2.0", "0.2.0-pr9"), Some(Ordering::Greater));
    }

    #[test]
    fn versions_compare() {
        use Ordering::*;
        assert_eq!(compare("0.2.0", "0.1.0"), Some(Greater));
        assert_eq!(compare("v0.10.0", "0.9.9"), Some(Greater));
        assert_eq!(compare("1.0", "1.0.0"), Some(Equal));
        assert_eq!(compare("0.1.0", "0.1.0"), Some(Equal));
        assert_eq!(compare("0.1.0", "0.2.0"), Some(Less));
        assert_eq!(compare("1.0.0-beta", "1.0.0"), Some(Less));
        assert_eq!(compare("1.0.1-beta", "1.0.0"), Some(Greater));
        assert_eq!(compare("nightly", "1.0.0"), None);
        assert_eq!(compare("1.2.3.4", "1.0.0"), None);
    }

    #[test]
    fn newer_release_is_reported() {
        assert_eq!(
            outcome(200, &release("v0.2.0", ""), "0.1.0"),
            Outcome::Newer {
                version: "0.2.0".into(),
                url: "https://github.com/Danijimzzz/OmniLoad/releases/tag/v0.2.0".into(),
                install: None,
            }
        );
    }

    #[test]
    fn same_or_older_release_is_up_to_date() {
        assert_eq!(
            outcome(200, &release("v0.1.0", ""), "0.1.0"),
            Outcome::UpToDate {
                latest: "0.1.0".into()
            }
        );
        assert!(matches!(
            outcome(200, &release("v0.0.9", ""), "0.1.0"),
            Outcome::UpToDate { .. }
        ));
    }

    #[test]
    fn odd_answers() {
        assert_eq!(outcome(404, "{}", "0.1.0"), Outcome::NoReleases);
        assert_eq!(
            outcome(200, &release("v9.0.0", r#","prerelease":true"#), "0.1.0"),
            Outcome::NoReleases
        );
        assert!(matches!(outcome(403, "", "0.1.0"), Outcome::Failed(_)));
        assert!(matches!(outcome(500, "", "0.1.0"), Outcome::Failed(_)));
        assert!(matches!(
            outcome(200, "no es json", "0.1.0"),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            outcome(200, &release("nightly", ""), "0.1.0"),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn link_outside_github_is_not_followed() {
        let body = r#"{"tag_name":"v0.3.0","html_url":"https://malo.example/x"}"#;
        assert_eq!(
            outcome(200, body, "0.1.0"),
            Outcome::Newer {
                version: "0.3.0".into(),
                url: releases_page(),
                install: None,
            }
        );
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!(
                "https://github.com/Danijimzzz/OmniLoad/releases/download/v0.5.0/{name}"
            ),
            size: 10,
        }
    }

    #[test]
    fn picks_the_installer_for_each_system() {
        let assets = [
            asset("OmniLoad-0.5.0-windows-x64-portable.zip"),
            asset("OmniLoad-Setup-0.5.0-windows-x64.exe"),
            asset("OmniLoad-0.5.0-macos-arm64.dmg"),
            asset("OmniLoad-0.5.0-macos-x64.dmg"),
            asset("OmniLoad-0.5.0-linux-x64.AppImage"),
            asset("OmniLoad-0.5.0-linux-x64.deb"),
            asset("SHA256SUMS.txt"),
        ];
        let name = |p| pick_install(&assets, p).map(|i| i.name);
        assert_eq!(
            name(Platform::WindowsX64).as_deref(),
            Some("OmniLoad-Setup-0.5.0-windows-x64.exe")
        );
        assert_eq!(
            name(Platform::MacArm64).as_deref(),
            Some("OmniLoad-0.5.0-macos-arm64.dmg")
        );
        assert_eq!(
            name(Platform::MacX64).as_deref(),
            Some("OmniLoad-0.5.0-macos-x64.dmg")
        );
        assert_eq!(
            name(Platform::LinuxAppImage).as_deref(),
            Some("OmniLoad-0.5.0-linux-x64.AppImage")
        );
        assert_eq!(name(Platform::Other), None);
        // Sin suma de comprobación no se instala nada.
        assert!(pick_install(&assets[..6], Platform::WindowsX64).is_none());
        // Un archivo que no viene de GitHub no vale.
        let mut evil = vec![
            asset("OmniLoad-Setup-0.5.0-windows-x64.exe"),
            asset("SHA256SUMS.txt"),
        ];
        evil[0].browser_download_url = "https://malo.example/x.exe".into();
        assert!(pick_install(&evil, Platform::WindowsX64).is_none());
    }

    #[test]
    fn finds_sums() {
        let h = "a".repeat(64);
        let text = format!(
            "{h}  OmniLoad-1.exe\n{} *otro.dmg\nbasura\n",
            "B".repeat(64)
        );
        assert_eq!(
            sum_for(&text, "OmniLoad-1.exe").as_deref(),
            Some(h.as_str())
        );
        assert_eq!(sum_for(&text, "otro.dmg").unwrap(), "b".repeat(64));
        assert_eq!(sum_for(&text, "falta.exe"), None);
        assert_eq!(sum_for("zz  OmniLoad-1.exe", "OmniLoad-1.exe"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn script_quotes_paths() {
        if cfg!(target_os = "linux") {
            // SAFETY: solo esta prueba toca la variable.
            unsafe { std::env::set_var("APPIMAGE", "/tmp/mi app's/OmniLoad.AppImage") };
            let s = unix_update_script(std::path::Path::new("/tmp/nuevo.AppImage"), 42).unwrap();
            assert!(s.contains("kill -0 42"));
            assert!(s.contains("'/tmp/mi app'\\''s/OmniLoad.AppImage'"), "{s}");
        }
    }

    #[test]
    fn api_address() {
        assert_eq!(
            api_url(),
            "https://api.github.com/repos/Danijimzzz/OmniLoad/releases/latest"
        );
    }
}
