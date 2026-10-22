mod awake;
mod bang;
mod cli;
mod config;
mod handoff;
mod keys;
mod launch;
mod pane;
mod probe;
mod render;
mod scan;
mod session;
mod term;
mod text;
mod turn;
mod update;

use std::path::Path;
use std::process::exit;

use cli::Command;

fn main() {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<String> = raw.iter().map(|a| a.to_string_lossy().into_owned()).collect();

    match cli::parse(&args) {
        Err(e) => {
            eprintln!("ai: {e}");
            exit(2);
        }
        Ok(Command::Help) => print!("{}", cli::help()),
        Ok(Command::Update { tools }) => exit(update::run(&tools)),
        Ok(Command::History { words }) => exit(config::print_history(&words)),
        Ok(Command::Run(opts)) => {
            // Answer to the agent's name, so a pane watcher reading argv[0]
            // sees `omp` and not `ai`. Only worth it when we stay around to
            // draw; otherwise the agent replaces us anyway.
            if std::env::var_os(term::REEXEC_ENV).is_none() && term::is_tty(0) && term::is_tty(1) {
                let name = Path::new(&opts.argv[0]).file_name().map_or("ai".into(), |n| n.to_string_lossy());
                let err = term::exec_as(&name, &raw);
                eprintln!("ai: could not rename the process ({err}), carrying on as ai");
            }
            if !opts.agent_files.is_empty() {
                let home = std::env::var("HOME").unwrap_or_default();
                let files: Vec<String> = opts
                    .agent_files
                    .iter()
                    .map(|f| {
                        let s = f.to_string_lossy();
                        match s.strip_prefix(&home) {
                            Some(rest) if !home.is_empty() => format!("~{rest}"),
                            _ => s.into_owned(),
                        }
                    })
                    .collect();
                println!("{}agents: {}{}", text::GRAY, files.join(", "), text::RESET);
            }
            exit(session::run(opts));
        }
    }
}
