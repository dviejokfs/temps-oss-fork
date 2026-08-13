//! Shared helper for ensuring a Docker image is locally cached before
//! `create_container`.
//!
//! Without this, fresh prod hosts 404 on `create_container` with
//! "No such image: …" because the engine assumed Docker would auto-pull
//! (it doesn't — bare `create_container` against a missing image fails).

use bollard::Docker;
use futures::stream::StreamExt as FuturesStreamExt;
use tracing::{debug, info};

use temps_backup_core::engine_v2::BackupError;

fn create_image_options(image_reference: &str) -> bollard::query_parameters::CreateImageOptions {
    use bollard::query_parameters::CreateImageOptionsBuilder;

    // Docker accepts the complete reference in `fromImage`, including tags,
    // registry ports, and digest pins. Splitting on `:` corrupts both
    // `registry.example:5000/image:tag` and `image:tag@sha256:...`; setting a
    // separate `tag` also overrides the tag embedded in `fromImage`.
    CreateImageOptionsBuilder::new()
        .from_image(image_reference)
        .build()
}

/// Ensure `image_tag` is pulled and available locally. No-op if Docker
/// already has the image cached; otherwise streams a pull and returns
/// after the daemon reports completion.
///
/// Pull failures are mapped to `BackupError::Failed` so the engine's caller
/// surfaces a useful message ("failed to pull '…': …") instead of a generic
/// 404 on the subsequent `create_container` call.
///
/// `engine` is the engine key (e.g. `"control_plane"`, `"postgres_pgdump"`);
/// errors carry it in logs so failures land on the right engine in the UI.
pub async fn ensure_image_pulled_v2(image_tag: &str, engine: &str) -> Result<(), BackupError> {
    let docker = Docker::connect_with_local_defaults().map_err(|e| BackupError::Failed {
        reason: format!(
            "ensure_image_pulled_v2 ({}): failed to connect to Docker: {}",
            engine, e
        ),
    })?;

    if docker.inspect_image(image_tag).await.is_ok() {
        return Ok(());
    }

    info!(
        image_tag,
        engine, "ensure_image_pulled_v2: image not cached, pulling"
    );

    let options = create_image_options(image_tag);

    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(result) = FuturesStreamExt::next(&mut stream).await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    debug!(image_tag, engine, "Docker pull: {}", status);
                }
            }
            Err(e) => {
                return Err(BackupError::Failed {
                    reason: format!("failed to pull '{}': {}", image_tag, e),
                });
            }
        }
    }

    info!(image_tag, engine, "ensure_image_pulled_v2: pull complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::create_image_options;

    #[test]
    fn digest_pinned_reference_is_passed_to_docker_intact() {
        let reference = "mongo:7.0.39-jammy@sha256:04582c3a144d088f841c446abfc19f79a";
        let options = create_image_options(reference);

        assert_eq!(options.from_image.as_deref(), Some(reference));
        assert_eq!(options.tag, None);
    }

    #[test]
    fn registry_port_and_tag_are_passed_to_docker_intact() {
        let reference = "registry.example.com:5000/team/image:v1";
        let options = create_image_options(reference);

        assert_eq!(options.from_image.as_deref(), Some(reference));
        assert_eq!(options.tag, None);
    }
}
