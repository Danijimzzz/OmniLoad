//! Aviso de versión nueva: al abrir (si no se desactiva) pregunta a GitHub
//! cuál es la última versión publicada y, si es más nueva que esta, avisa.
//! Nunca descarga ni instala nada: solo enseña un aviso y un botón que abre la
//! página de la versión en el navegador.
//!
//! Solo se envía una petición `GET` a la API pública de GitHub, con el
//! nombre y número de versión del programa en la cabecera `User-Agent` (lo
//! exige GitHub) y nada más: ni identificador, ni datos del equipo.

use std::cmp::Ordering;
use std::time::Duration;

use serde::Deserialize;

/// La versión de este programa.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Qué se encontró.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Hay una versión más nueva.
    Newer { version: String, url: String },
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
    match compare(&latest, current) {
        Some(Ordering::Greater) if rel.html_url.starts_with("https://github.com/") => {
            Outcome::Newer {
                version: latest,
                url: rel.html_url,
            }
        }
        Some(Ordering::Greater) => Outcome::Newer {
            version: latest,
            url: releases_page(),
        },
        Some(_) => Outcome::UpToDate { latest },
        None => Outcome::Failed(format!("versión «{}» no reconocida", rel.tag_name)),
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
                url: "https://github.com/Danijimzzz/OmniLoad/releases/tag/v0.2.0".into()
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
                url: releases_page()
            }
        );
    }

    #[test]
    fn api_address() {
        assert_eq!(
            api_url(),
            "https://api.github.com/repos/Danijimzzz/OmniLoad/releases/latest"
        );
    }
}
