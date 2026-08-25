use std::{env, fs, path::PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("marketplace sync preparation failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> dsh_core::AppResult<()> {
    let mut arguments = env::args_os().skip(1);
    let output = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| dsh_core::AppError::new("marketSyncOutputMissing"))?;
    let current = arguments.next().map(PathBuf::from);
    if arguments.next().is_some() {
        return Err(dsh_core::AppError::new("marketSyncArgumentsInvalid"));
    }
    let current_bytes = current
        .as_ref()
        .map(fs::read)
        .transpose()
        .map_err(|error| dsh_core::AppError::io("marketSyncCurrentReadFailed", &error))?;
    let token =
        env::var("GITHUB_TOKEN").map_err(|_| dsh_core::AppError::new("marketSyncTokenMissing"))?;
    match dsh_core::marketplace::prepare_marketplace_publication(current_bytes.as_deref(), &token)?
    {
        Some(publication) => {
            publication.write_to(&output)?;
            println!(
                "prepared={} commit={} sha256={}",
                publication.manifest.slot, publication.manifest.commit, publication.manifest.sha256
            );
        }
        None => println!("unchanged"),
    }
    Ok(())
}
