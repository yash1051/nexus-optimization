use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum TelemetrySubcommand {
    Status,
    Enable,
    Disable,
    Forget,
}

pub fn run(command: &TelemetrySubcommand) -> Result<()> {
    match command {
        TelemetrySubcommand::Status => run_status(),
        TelemetrySubcommand::Enable => run_enable(),
        TelemetrySubcommand::Disable => run_disable(),
        TelemetrySubcommand::Forget => run_forget(),
    }
}

fn run_status() -> Result<()> {
    let config = crate::core::config::Config::load().unwrap_or_default();

    let consent_str = match config.telemetry.consent_given {
        Some(true) => "yes",
        Some(false) => "no",
        None => "never asked",
    };

    let enabled_str = if config.telemetry.enabled {
        "yes"
    } else {
        "no"
    };

    let env_override = std::env::var("RTK_TELEMETRY_DISABLED").unwrap_or_default() == "1";

    println!("Telemetry status:");
    println!("  consent:       {}", consent_str);
    if let Some(date) = &config.telemetry.consent_date {
        println!("  consent date:  {}", date);
    }
    println!("  enabled:       {}", enabled_str);
    if env_override {
        println!("  env override:  blocked");
    }

    println!();
    println!("Telemetry has been removed from this build. No data is collected");
    println!("or transmitted from your machine. All command analytics stay local");
    println!("and are visible via `nexus gain`.");

    Ok(())
}

fn run_enable() -> Result<()> {
    // Telemetry has been removed from this build. The command is kept for
    // compatibility so existing docs/scripts don't error, but enabling is a
    // no-op — there is no endpoint and no sender.
    crate::hooks::init::save_telemetry_consent(false).ok();
    println!("Telemetry is not available in this build. Nothing was enabled.");
    println!("All command analytics stay local — see `nexus gain`.");
    Ok(())
}

fn run_disable() -> Result<()> {
    crate::hooks::init::save_telemetry_consent(false)?;
    println!("Telemetry disabled.");
    Ok(())
}

fn run_forget() -> Result<()> {
    // No remote telemetry in this build — only local data exists to forget.
    crate::hooks::init::save_telemetry_consent(false).ok();

    let salt_path = super::telemetry::salt_file_path();
    let marker_path = super::telemetry::telemetry_marker_path();

    if salt_path.exists() {
        std::fs::remove_file(&salt_path)
            .with_context(|| format!("Failed to delete {}", salt_path.display()))?;
    }

    if marker_path.exists() {
        let _ = std::fs::remove_file(&marker_path);
    }

    // Purge local tracking database (right to erasure applies to local data too).
    let db_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(super::constants::RTK_DATA_DIR)
        .join(super::constants::HISTORY_DB);
    if db_path.exists() {
        match std::fs::remove_file(&db_path) {
            Ok(()) => println!("Local tracking database deleted: {}", db_path.display()),
            Err(e) => eprintln!("nexus: could not delete {}: {}", db_path.display(), e),
        }
    }

    println!("All local telemetry and tracking data deleted.");
    println!("(No remote endpoint exists in this build — nothing was ever sent.)");
    Ok(())
}
