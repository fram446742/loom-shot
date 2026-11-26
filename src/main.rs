use std::path::PathBuf;
use std::process::ExitCode;

use loom_shot::cli::{self, CliAction};
use loom_shot::{editor, history, ocr, pin, record, run};

fn main() -> ExitCode {
    match run_cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("loom-shot: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli() -> Result<(), String> {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let parsed = match cli::parse(std::env::args().skip(1))? {
        CliAction::Help => {
            print!("{}", cli::USAGE);
            print!("\n{}", record::USAGE);
            print!("\n{}", editor::USAGE);
            print!("\n{}", history::USAGE);
            print!("\n{}", pin::USAGE);
            print!("\n{}", ocr::USAGE);
            return Ok(());
        }
        CliAction::Run(parsed) => parsed,
        CliAction::Record(options) => {
            let outcome = record::run(&record::Env::from_process(), &options)?;
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome.to_json()).unwrap()
                );
            } else {
                println!("{}", outcome.summary());
            }
            return Ok(());
        }
        CliAction::Annotate(options) => {
            println!("{}", editor::run(&options)?);
            return Ok(());
        }
        CliAction::History(options) => {
            println!("{}", history::run(&options)?);
            return Ok(());
        }
        CliAction::Pin(options) => {
            println!("{}", pin::run(&options)?);
            return Ok(());
        }
        CliAction::Ocr(options) => {
            println!("{}", ocr::run(&options)?);
            return Ok(());
        }
    };
    let socket = parsed
        .socket
        .or_else(|| std::env::var_os("NIRI_SOCKET").map(PathBuf::from))
        .ok_or_else(|| "NIRI_SOCKET is not set; run inside niri or pass --socket".to_owned())?;
    let outcome = run(&socket, &parsed.options)?;
    let captures = outcome
        .captures
        .iter()
        .map(|capture| {
            serde_json::json!({
                "path": capture.path,
                "output": capture.output,
                "format": capture.format.name(),
                "width": capture.width,
                "height": capture.height,
                "source_color": capture.source_color,
                "metadata_path": capture.metadata_path,
                "hdr": capture.hdr.to_json(),
                "ocr": capture.ocr.as_ref().map(|result| result.to_json(&capture.path)),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "path": outcome.path,
            "format": outcome.format.name(),
            "copied": outcome.copied,
            "annotated": outcome.annotated,
            "metadata_path": outcome.metadata_path,
            "width": outcome.width,
            "height": outcome.height,
            "source_color": outcome.source_color,
            "hdr": outcome.hdr.to_json(),
            "action": outcome.action,
            "actions": outcome.actions,
            "captures": captures,
            "stitched": outcome.stitched,
            "ocr": outcome.ocr.as_ref().map(|result| result.to_json(&outcome.path)),
            "history_path": outcome.history_path,
            "dry_run": parsed.options.dry_run,
        }))
        .unwrap()
    );
    Ok(())
}
