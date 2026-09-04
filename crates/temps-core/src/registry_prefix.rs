// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Detects implicit Docker Hub image references and rewrites them through an
//! operator-configured registry mirror/prefix.
//!
//! Every base image Temps builds against (autopack's generated Dockerfiles,
//! external service images) is normally an unqualified reference like
//! `node:22-slim` or `debian:bookworm-slim` — Docker resolves those against
//! `docker.io` with no credentials attached, and Docker Hub throttles
//! anonymous pulls by source IP. Some operators run an internal registry that
//! is a path-prefixing reverse proxy rather than a `registry-mirrors`
//! (pull-through cache) protocol implementation, so the daemon-level
//! `registry-mirrors` option (see `docs/howto/configure-a-docker-registry-mirror`)
//! doesn't work for them — the only way to route through their proxy is to
//! rewrite the reference itself before it reaches the daemon.
//!
//! This module is the single place that decides "is this reference implicit
//! Docker Hub" and "what does it become under the configured prefix", so
//! every call site (autopack Dockerfile generation, direct image pulls)
//! agrees on the same rule.

/// Returns `true` when `image` has no explicit registry host, i.e. Docker
/// would resolve it against `docker.io`.
///
/// Mirrors the rule Docker's own reference parser uses: the first path
/// segment (up to the first `/`) is a registry host only if it contains a
/// `.` or `:`, or is exactly `localhost`. A bare `library/postgres` or
/// `bitnami/postgresql` has no such segment and is still Docker Hub; `node`
/// (no `/` at all) is the official-image shorthand and is always Docker Hub
/// regardless of a tag's `:` (the tag separator is not a host separator when
/// there is no `/` in the reference at all).
pub fn is_docker_hub_image(image: &str) -> bool {
    let Some((first_segment, _rest)) = image.split_once('/') else {
        // No slash at all: either "node" or "node:22-slim" — both are the
        // official-image shorthand on docker.io. Never mistake the tag's `:`
        // for a host port here.
        return true;
    };

    let looks_like_registry_host =
        first_segment.contains('.') || first_segment.contains(':') || first_segment == "localhost";

    !looks_like_registry_host
}

/// Rewrite `image` through `prefix` if it is an implicit Docker Hub
/// reference and a prefix is configured; otherwise return it unchanged.
///
/// The rewrite is a plain concatenation (`{prefix}/{image}`), matching how
/// operators' existing registry proxies already rewrite other tooling's
/// image references — it does not expand the implicit `library/` namespace,
/// since a proxy that accepts `<prefix>/gotempsh/temps` is expected to accept
/// `<prefix>/node` the same way.
pub fn qualify_with_registry_prefix(image: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) if !prefix.is_empty() && is_docker_hub_image(image) => {
            format!("{}/{}", prefix.trim_end_matches('/'), image)
        }
        _ => image.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_implicit_docker_hub_references() {
        assert!(is_docker_hub_image("node:22-slim"));
        assert!(is_docker_hub_image("node"));
        assert!(is_docker_hub_image("debian:bookworm-slim"));
        assert!(is_docker_hub_image("bitnami/postgresql:16"));
        assert!(is_docker_hub_image("library/node:22-slim"));
    }

    #[test]
    fn recognises_already_qualified_references_as_not_docker_hub() {
        assert!(!is_docker_hub_image("ghcr.io/gotempsh/temps:latest"));
        assert!(!is_docker_hub_image("quay.io/coreos/etcd:v3.5.0"));
        assert!(!is_docker_hub_image("localhost:5000/myimage:latest"));
        assert!(!is_docker_hub_image(
            "registry.example.com:5000/team/app:latest"
        ));
    }

    #[test]
    fn qualifies_only_docker_hub_references_when_a_prefix_is_configured() {
        assert_eq!(
            qualify_with_registry_prefix("node:22-slim", Some("registry.example.com/docker")),
            "registry.example.com/docker/node:22-slim"
        );
        assert_eq!(
            qualify_with_registry_prefix(
                "gotempsh/temps:latest",
                Some("registry.example.com/docker")
            ),
            "registry.example.com/docker/gotempsh/temps:latest"
        );

        // Already-qualified references pass through untouched even with a
        // prefix configured — rewriting them would point at the wrong host.
        assert_eq!(
            qualify_with_registry_prefix(
                "ghcr.io/gotempsh/temps:latest",
                Some("registry.example.com/docker")
            ),
            "ghcr.io/gotempsh/temps:latest"
        );
    }

    #[test]
    fn leaves_images_unchanged_when_no_prefix_is_configured() {
        assert_eq!(
            qualify_with_registry_prefix("node:22-slim", None),
            "node:22-slim"
        );
        assert_eq!(
            qualify_with_registry_prefix("node:22-slim", Some("")),
            "node:22-slim"
        );
    }

    #[test]
    fn trims_a_trailing_slash_on_the_configured_prefix() {
        assert_eq!(
            qualify_with_registry_prefix("node:22-slim", Some("registry.example.com/docker/")),
            "registry.example.com/docker/node:22-slim"
        );
    }
}
