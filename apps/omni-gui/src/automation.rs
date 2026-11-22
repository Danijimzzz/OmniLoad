//! Lo que OmniLoad hace solo: la acción al terminar todas las descargas y el
//! inicio con el sistema.
//!
//! [`FinishWatch`] mira las fotos de la cola y dice cuándo se acabó todo; no
//! depende de Slint ni del sistema, así se puede probar. [`run_action`] y
//! [`set_autostart`] tocan el sistema, cada uno a su manera según el sistema
//! operativo.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use crate::manager::{RowInfo, State};

/// Qué hacer al terminar todo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishAction {
    Nothing,
    /// Cerrar OmniLoad.
    Quit,
    Suspend,
    Shutdown,
}

impl FinishAction {
    /// Posición del desplegable de Ajustes.
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Quit,
            2 => Self::Suspend,
            3 => Self::Shutdown,
            _ => Self::Nothing,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Nothing => "",
            Self::Quit => "cerrar OmniLoad",
            Self::Suspend => "suspender el equipo",
            Self::Shutdown => "apagar el equipo",
        }
    }
}

/// Dice cuándo se acaba todo lo que estaba descargándose o esperando.
#[derive(Debug, Default)]
pub struct FinishWatch {
    busy: HashSet<u64>,
}

impl FinishWatch {
    /// `true` una sola vez cuando termina la última descarga de la tanda.
    /// No salta si el usuario pausó algo ni si todo lo que había se quitó.
    pub fn update(&mut self, rows: &[RowInfo]) -> bool {
        let waiting = |s: State| s.is_active() || s == State::Queued;
        let mut any_busy = false;
        for r in rows {
            if waiting(r.state) {
                any_busy = true;
                self.busy.insert(r.id);
            }
        }
        if any_busy || self.busy.is_empty() {
            return false;
        }
        let states: Vec<State> = self
            .busy
            .iter()
            .filter_map(|id| rows.iter().find(|r| r.id == *id).map(|r| r.state))
            .collect();
        self.busy.clear();
        !states.is_empty()
            && states
                .iter()
                .all(|s| matches!(s, State::Done | State::Failed))
            && states.contains(&State::Done)
    }
}

/// Suspende o apaga el equipo.
pub fn run_action(action: FinishAction) {
    let result = match action {
        FinishAction::Suspend => suspend(),
        FinishAction::Shutdown => shutdown(),
        FinishAction::Nothing | FinishAction::Quit => return,
    };
    if let Err(e) = result {
        tracing::warn!("no se pudo {}: {e}", action.describe());
    }
}

/// Un comando del sistema. En Windows sin ventana de consola: sin esto, cada
/// llamada hace aparecer y desaparecer una ventana negra.
fn command(program: &str) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn run(program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = command(program).args(args).status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other(format!("{program} terminó con {status}")))
}

#[cfg(windows)]
fn suspend() -> std::io::Result<()> {
    run("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"])
}
#[cfg(windows)]
fn shutdown() -> std::io::Result<()> {
    run("shutdown", &["/s", "/t", "0"])
}
#[cfg(target_os = "macos")]
fn suspend() -> std::io::Result<()> {
    run("pmset", &["sleepnow"])
}
#[cfg(target_os = "macos")]
fn shutdown() -> std::io::Result<()> {
    run(
        "osascript",
        &["-e", "tell app \"System Events\" to shut down"],
    )
}
#[cfg(not(any(windows, target_os = "macos")))]
fn suspend() -> std::io::Result<()> {
    run("systemctl", &["suspend"])
}
#[cfg(not(any(windows, target_os = "macos")))]
fn shutdown() -> std::io::Result<()> {
    run("systemctl", &["poweroff"])
}

// ---- Inicio con el sistema ----

/// Argumento con el que arranca la ventana al iniciar el sistema: sale
/// escondida en la bandeja.
pub const AUTOSTART_FLAG: &str = "--minimizado";

/// ¿Está puesto el inicio con el sistema?
pub fn autostart_enabled() -> bool {
    imp::enabled()
}

/// Pone o quita el inicio con el sistema.
pub fn set_autostart(on: bool) -> std::io::Result<()> {
    imp::set(on)
}

#[allow(dead_code)]
fn exe() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

#[cfg(windows)]
mod imp {
    use super::*;
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "OmniLoad";

    pub fn enabled() -> bool {
        command("reg")
            .args(["query", KEY, "/v", NAME])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        if on {
            let value = format!("\"{}\" {AUTOSTART_FLAG}", exe()?.display());
            run(
                "reg",
                &["add", KEY, "/v", NAME, "/t", "REG_SZ", "/d", &value, "/f"],
            )
        } else if enabled() {
            run("reg", &["delete", KEY, "/v", NAME, "/f"])
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn plist() -> Option<PathBuf> {
        dirs_home().map(|h| h.join("Library/LaunchAgents/io.github.danijimzzz.omniload.plist"))
    }

    fn dirs_home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    pub fn enabled() -> bool {
        plist().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let path = plist().ok_or_else(|| std::io::Error::other("sin carpeta de usuario"))?;
        if on {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
<key>Label</key><string>io.github.danijimzzz.omniload</string>\n\
<key>ProgramArguments</key><array><string>{}</string><string>{AUTOSTART_FLAG}</string></array>\n\
<key>RunAtLoad</key><true/>\n\
</dict></plist>\n",
                exe()?.display()
            );
            std::fs::write(path, body)
        } else {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;

    fn desktop_file() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart/omniload.desktop"))
    }

    pub fn enabled() -> bool {
        desktop_file().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let path = desktop_file().ok_or_else(|| std::io::Error::other("sin carpeta de usuario"))?;
        if on {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // Las comillas protegen una ruta con espacios.
            let body = format!(
                "[Desktop Entry]\nType=Application\nName=OmniLoad\nComment=Gestor de descargas\nExec=\"{}\" {AUTOSTART_FLAG}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe()?.display()
            );
            std::fs::write(path, body)
        } else {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Priority;

    fn row(id: u64, state: State) -> RowInfo {
        RowInfo {
            id,
            package: "p".into(),
            name: format!("a{id}"),
            host: "a.com".into(),
            folder: "/tmp".into(),
            url: format!("https://a.com/{id}"),
            state,
            priority: Priority::Normal,
            held: false,
            downloaded: 0,
            total: None,
            rate: 0.0,
            receiving: 0,
            connections: 0,
            server_limit: 0,
            retries: 0,
            started_once: true,
            speed_limit: None,
            detail: String::new(),
        }
    }

    #[test]
    fn fires_once_when_the_last_download_ends() {
        let mut w = FinishWatch::default();
        assert!(!w.update(&[row(1, State::Running), row(2, State::Queued)]));
        assert!(!w.update(&[row(1, State::Done), row(2, State::Running)]));
        assert!(w.update(&[row(1, State::Done), row(2, State::Done)]));
        assert!(!w.update(&[row(1, State::Done), row(2, State::Done)]));
    }

    #[test]
    fn nothing_when_paused_removed_or_already_finished() {
        let mut w = FinishWatch::default();
        assert!(!w.update(&[row(1, State::Done)]));
        w.update(&[row(1, State::Running)]);
        assert!(!w.update(&[row(1, State::Paused)]));
        w.update(&[row(1, State::Running)]);
        assert!(!w.update(&[]));
        w.update(&[row(1, State::Running)]);
        assert!(!w.update(&[row(1, State::Failed)]));
    }

    #[test]
    fn action_from_index() {
        assert_eq!(FinishAction::from_index(0), FinishAction::Nothing);
        assert_eq!(FinishAction::from_index(3), FinishAction::Shutdown);
    }
}
