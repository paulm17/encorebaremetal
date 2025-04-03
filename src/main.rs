use std::env;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;
use serde_json::{Value, from_reader};
use tempfile::Builder;
use walkdir::WalkDir;

struct Config {
    debug: bool,
}

impl Config {
    fn new() -> Self {
        let debug = env::var("DEBUG").map(|v| v == "1").unwrap_or(false);
        Self { debug }
    }
    fn log(&self, message: &str) {
        if self.debug {
            println!("{}", message);
        }
    }
    fn log_fmt(&self, args: std::fmt::Arguments<'_>) {
        if self.debug {
            println!("{}", args);
        }
    }
}

/// Runs a command with optional working directory and returns its output.
fn run_command(
    program: &str,
    args: &[&str],
    work_dir: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Command {} {:?} failed: {}",
            program,
            args,
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

/// Build the docker image using the 'encore' executable.
fn docker_build(image_tag: &str, config: &Config) -> Result<String, Box<dyn std::error::Error>> {
    println!("Building Docker image {}...", image_tag);
    // Locate the 'encore' executable.
    let which_output = Command::new("which").arg("encore").output()?;
    if !which_output.status.success() {
        return Err(format!(
            "Failed to locate 'encore': {}",
            String::from_utf8_lossy(&which_output.stderr)
        )
        .into());
    }
    let encore_path = String::from_utf8(which_output.stdout)?.trim().to_string();

    run_command(&encore_path, &["build", "docker", image_tag], None)?;
    config.log("Docker image built successfully.");
    Ok(encore_path)
}

/// Save the docker image to a tar file.
fn docker_save(image_tag: &str, tar_path: &Path, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    config.log_fmt(format_args!("Saving Docker image to {}...", tar_path.display()));
    run_command("docker", &["save", "-o", tar_path.to_str().unwrap(), image_tag], None)?;
    println!("Saved Docker image successfully.");
    Ok(())
}

/// Remove docker images.
fn docker_remove(image_tag: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    config.log_fmt(format_args!("Removing Docker images node:slim and {}", image_tag));
    run_command("docker", &["image", "rm", "node:slim", image_tag], None)?;
    println!("Docker images removed successfully.");
    Ok(())
}

/// Parse manifest.json to obtain the digest of the largest layer.
fn parse_manifest(manifest_path: &Path, config: &Config) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(manifest_path)?;
    let manifest: Vec<Value> = from_reader(file)?;
    let layer_sources = &manifest[0]["LayerSources"];
    let mut largest_layer = None;
    let mut largest_size = 0u64;

    if let Value::Object(sources) = layer_sources {
        for (digest, info) in sources {
            if let Some(size) = info.get("size").and_then(|s| s.as_u64()) {
                if size > largest_size {
                    largest_size = size;
                    largest_layer = Some(digest.clone());
                }
            }
        }
    }
    let digest = largest_layer.ok_or("No layer sources found")?;
    config.log_fmt(format_args!("Selected largest layer ({} bytes): {}", largest_size, digest));
    // Remove "sha256:" prefix if present.
    Ok(if digest.starts_with("sha256:") {
        digest[7..].to_string()
    } else {
        digest
    })
}

/// Copies a directory recursively. If `exclusions` is provided, paths matching any exclusion are skipped.
fn copy_dir(src: &Path, dst: &Path, exclusions: Option<&[&str]>, debug: bool) -> io::Result<()> {
    if !src.exists() || !src.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Source not found: {}", src.display())));
    }
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let path = entry.path();
        let rel_path = path.strip_prefix(src).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if let Some(exclusions) = exclusions {
            let rel_str = rel_path.to_string_lossy();
            if exclusions.iter().any(|&ex| rel_str == ex || rel_str.starts_with(&format!("{}/", ex))) {
                if debug {
                    println!("Skipping excluded: {}", rel_path.display());
                }
                continue;
            }
        }
        let target = dst.join(rel_path);
        if path.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target).unwrap_or_else(|e| {
                if debug {
                    println!("Warning: Failed to copy {}: {}", path.display(), e);
                }
                0 // continue on error
            });
        }
    }
    Ok(())
}

/// High-level routine to copy components from the extracted 'encore' directory into the output directory.
fn copy_encore_components(encore_dir: &Path, output_dir: &Path, config: &Config) -> io::Result<()> {
    // Example: copy artifacts/0 (with exclusions), build folder, node_modules (with exclusions and symlinks), and runtimes.
    if encore_dir.exists() && encore_dir.is_dir() {
        config.log_fmt(format_args!("Found encore at: {}", encore_dir.display()));
        // Try to find the workspace/apps/encore/.encore directory
        if let Some(parent) = encore_dir.parent() {
            let encore_config_dir = parent.join("workspace/apps/encore/.encore");
            if encore_config_dir.exists() && encore_config_dir.is_dir() {
                // Create artifacts directory
                let artifacts_dir = output_dir.join("artifacts");
                fs::create_dir_all(&artifacts_dir)?;

                // Copy build folder into artifacts
                let build_dir = encore_config_dir.join("build");
                if build_dir.exists() && build_dir.is_dir() {
                    let target_build = artifacts_dir.join("build");
                    copy_dir(&build_dir, &target_build, None, config.debug)?;
                    config.log_fmt(format_args!("Copied build to {}", target_build.display()));
                } else {
                    config.log("Warning: build directory not found");
                }

                // Copy manifest.json
                let manifest_file = encore_config_dir.join("manifest.json");
                if manifest_file.exists() {
                    let target_manifest = artifacts_dir.join("manifest.json");
                    if let Some(parent) = target_manifest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&manifest_file, &target_manifest)?;
                    config.log_fmt(format_args!("Copied manifest.json to {}", target_manifest.display()));
                } else {
                    config.log("Warning: manifest.json not found");
                }
            } else {
                config.log("Warning: .encore directory not found in expected location");
            }
        }
        // Copy runtimes directory.
        let runtimes_dir = encore_dir.join("runtimes");
        if runtimes_dir.exists() && runtimes_dir.is_dir() {
            let target_runtimes = output_dir.join("runtimes");
            copy_dir(&runtimes_dir, &target_runtimes, None, config.debug)?;
            config.log_fmt(format_args!("Copied runtimes to {}", target_runtimes.display()));
        } else {
            config.log("Warning: runtimes directory not found in encore");
        }
        // Copy specific files.
        let files = ["build-info.json", "infra.config.json", "meta"];
        for file in &files {
            let source = encore_dir.join(file);
            if source.exists() {
                let target = output_dir.join(file);
                if source.is_dir() {
                    copy_dir(&source, &target, None, config.debug)?;
                } else {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&source, &target)?;
                }
                config.log_fmt(format_args!("Copied {} to {}", file, target.display()));
            } else {
                config.log_fmt(format_args!("Warning: {} not found", file));
            }
        }
    } else {
        config.log("Error: encore directory not found in extracted layer");
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::new();
    let args: Vec<String> = env::args().collect();
    let image_tag = args
        .windows(2)
        .find(|w| w[0] == "--image")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "my_image:latest".to_string());
    
    let current_dir = env::current_dir()?;
    config.log_fmt(format_args!("Current working directory: {}", current_dir.display()));

    // Clean old output directory.
    let old_output = current_dir.join("extracted_output");
    if old_output.exists() {
        config.log("Removing old extracted_output directory...");
        fs::remove_dir_all(&old_output)?;
    }
    
    // Define tar file path.
    let tar_path = current_dir.join("encoredocker.tar");
    
    // Docker build, save, and remove.
    let _encore_path = docker_build(&image_tag, &config)?;
    docker_save(&image_tag, &tar_path, &config)?;
    docker_remove(&image_tag, &config)?;

    // Create a temporary directory for extraction.
    let temp_base = current_dir.join("docker_extract_temp");
    fs::create_dir_all(&temp_base)?;
    let temp_dir = Builder::new().prefix("work_").tempdir_in(&temp_base)?;
    config.log_fmt(format_args!("Temporary directory created: {}", temp_dir.path().display()));

    // Extract tar into temporary directory.
    run_command("tar", &["xf", tar_path.to_str().unwrap()], Some(temp_dir.path()))?;

    // Parse manifest.json.
    let manifest_path = temp_dir.path().join("manifest.json");
    let layer_digest = parse_manifest(&manifest_path, &config)?;

    // Determine layer file path.
    let layer_path = temp_dir.path().join("blobs/sha256").join(&layer_digest);
    config.log_fmt(format_args!("Extracting layer from: {}", layer_path.display()));

    // Create directory for layer extraction.
    let layer_dir = Builder::new().prefix("layer_").tempdir_in(&temp_base)?;
    // Try extracting with gzip; if fails, fallback.
    let tar_extract_res = run_command("tar", &["xzvf", layer_path.to_str().unwrap()], Some(layer_dir.path()));
    if tar_extract_res.is_err() {
        config.log("Gzip extraction failed, trying regular extraction...");
        run_command("tar", &["xvf", layer_path.to_str().unwrap()], Some(layer_dir.path()))?;
    }

    // Create final output directory.
    let final_output = current_dir.join("encore_prod");
    fs::create_dir_all(&final_output)?;
    config.log_fmt(format_args!("Created output directory: {}", final_output.display()));

    // Attempt to copy required components from the 'encore' directory.
    let encore_dir = layer_dir.path().join("encore");
    if encore_dir.exists() && encore_dir.is_dir() {
        copy_encore_components(&encore_dir, &final_output, &config)?;
    } else {
        config.log("Error: 'encore' directory not found; searching recursively...");
        // (Recursive search logic could be factored out similarly.)
        let mut found = false;
        for entry in WalkDir::new(layer_dir.path()).into_iter().filter_map(|e| e.ok()) {
            if entry.file_name() == "encore" && entry.path().is_dir() {
                config.log_fmt(format_args!("Found encore at: {}", entry.path().display()));
                copy_encore_components(entry.path(), &final_output, &config)?;
                found = true;
            }
        }
        if !found {
            config.log("Could not locate any 'encore' directory in the extracted layer.");
        }
    }

    println!("Process completed! Files extracted to: {}", final_output.display());

    // Clean up temporary directories and tar file.
    println!("Cleaning up temporary files...");
    fs::remove_dir_all(&temp_base)?;
    if tar_path.exists() {
        println!("Removing tar file...");
        fs::remove_file(&tar_path)?;
    }
    Ok(())
}
