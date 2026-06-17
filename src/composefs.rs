use anyhow::{Context, Result, anyhow};
use std::process::Command;

pub fn pull_image(image_ref: &str) -> Result<String> {
    // Only add docker:// transport if the image reference doesn't already
    // have an explicit transport prefix (e.g. docker://, containers-storage:,
    // oci-archive:). A lone colon (e.g. a port number in a registry URL) is
    // not a transport prefix.
    let final_ref = if image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    };

    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "pull", &final_ref])
        .output()
        .context("failed to execute bootc internals cfs oci pull")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("pull failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.to_string())
}

pub fn create_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args([
            "internals",
            "cfs",
            "--system",
            "oci",
            "create-image",
            image_id,
        ])
        .output()
        .context("failed to execute bootc internals cfs oci create-image")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("create-image failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

pub fn seal_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "seal", image_id])
        .output()
        .context("failed to execute bootc internals cfs oci seal")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("seal failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.to_string())
}
