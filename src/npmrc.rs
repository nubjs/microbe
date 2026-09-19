//! The subset of `.npmrc` an installer needs, read from an EXPLICIT path only: the default
//! registry, per-scope registries, and credentials keyed by URL prefix — npm's "nerf dart",
//! `//host/path/`, which a credential applies to when the request URL starts with it.
//! Values are taken literally. `${VAR}` is NOT expanded, because this crate reads nothing
//! from the environment: an embedder resolves it and passes the result through
//! [`crate::Microbe::npmrc_contents`]. A consumed key that still holds one is an error
//! rather than a token sent verbatim.

use crate::error::Error;
use base64::Engine;
use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq)]
pub struct Npmrc {
    pub registry: Option<String>,
    /// `@scope` → registry URL, no trailing slash.
    pub scoped: BTreeMap<String, String>,
    /// `(nerf dart, Authorization header value)`.
    pub auth: Vec<(String, String)>,
}

pub fn parse(contents: &str) -> Result<Npmrc, Error> {
    let mut rc = Npmrc::default();
    let mut users = BTreeMap::new();
    let mut passwords = BTreeMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        let literal = || -> Result<String, Error> {
            if value.contains("${") {
                return Err(Error::Npmrc(format!(
                    "`{key}` uses `${{VAR}}`, which is not expanded; pass the resolved contents"
                )));
            }
            Ok(value.to_string())
        };
        if key == "registry" {
            rc.registry = Some(literal()?.trim_end_matches('/').to_string());
        } else if let Some(scope) = key.strip_suffix(":registry").filter(|s| s.starts_with('@')) {
            rc.scoped.insert(
                scope.to_string(),
                literal()?.trim_end_matches('/').to_string(),
            );
        } else if let Some((nerf, field)) =
            key.rsplit_once(':').filter(|(n, _)| n.starts_with("//"))
        {
            let nerf = if nerf.ends_with('/') {
                nerf.to_string()
            } else {
                format!("{nerf}/")
            };
            match field {
                "_authToken" => rc.auth.push((nerf, format!("Bearer {}", literal()?))),
                "_auth" => rc.auth.push((nerf, format!("Basic {}", literal()?))),
                "username" => {
                    users.insert(nerf, literal()?);
                }
                "_password" => {
                    passwords.insert(nerf, literal()?);
                }
                _ => {}
            }
        }
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    for (nerf, user) in users {
        if let Some(password) = passwords.get(&nerf) {
            let password = b64
                .decode(password)
                .map_err(|_| Error::Npmrc(format!("`{nerf}:_password` is not base64")))?;
            let pair = format!("{user}:{}", String::from_utf8_lossy(&password));
            rc.auth.push((nerf, format!("Basic {}", b64.encode(pair))));
        }
    }
    Ok(rc)
}

/// The URL-prefix form credentials are keyed by: scheme dropped, query dropped, the path
/// cut after its last `/`. `https://r.io/@s%2fx?x=1` → `//r.io/`;
/// `https://r.io/x/-/x-1.tgz` → `//r.io/x/-/`.
pub fn nerf(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let end = rest.rfind('/').map_or(rest.len(), |i| i + 1);
    let mut nerf = format!("//{}", &rest[..end]);
    if !nerf.ends_with('/') {
        nerf.push('/');
    }
    nerf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_registries_and_every_credential_form() {
        let rc = parse(
            "# comment\nregistry = \"https://r.io/\"\n@acme:registry=https://acme.io\n\
             //acme.io/:_authToken=tok\n//legacy.io/:_auth=YWI=\n\
             //basic.io:8080/:username=u\n//basic.io:8080/:_password=cHc=\nunrelated=${HOME}\n",
        )
        .unwrap();
        assert_eq!(rc.registry.as_deref(), Some("https://r.io"));
        assert_eq!(rc.scoped["@acme"], "https://acme.io");
        assert_eq!(
            rc.auth,
            vec![
                ("//acme.io/".to_string(), "Bearer tok".to_string()),
                ("//legacy.io/".to_string(), "Basic YWI=".to_string()),
                ("//basic.io:8080/".to_string(), "Basic dTpwdw==".to_string()),
            ]
        );
    }

    #[test]
    fn an_unexpanded_variable_in_a_consumed_key_is_an_error() {
        let err = parse("//r.io/:_authToken=${NPM_TOKEN}\n").unwrap_err();
        assert!(matches!(err, Error::Npmrc(_)), "{err}");
    }

    #[test]
    fn nerf_dart_matches_npm() {
        assert_eq!(nerf("https://r.io/@s%2fx?x=1"), "//r.io/");
        assert_eq!(nerf("https://r.io/x/-/x-1.tgz"), "//r.io/x/-/");
        assert_eq!(nerf("https://r.io"), "//r.io/");
        assert_eq!(nerf("http://h:8080/a/b"), "//h:8080/a/");
    }
}
