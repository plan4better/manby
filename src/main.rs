//! The `manby` command, which accepts tippecanoe's options, and its
//! subcommands for tippecanoe's other programs: `manby join` accepts
//! tile-join's, and `manby decode`, `manby enumerate`, `manby overzoom` and
//! `manby json-tool` are tippecanoe-rs's tippecanoe-decode,
//! tippecanoe-enumerate, tippecanoe-overzoom and tippecanoe-json-tool. Each
//! program's own name also works as the subcommand, and run under that name
//! (a link, say), manby is that program. `manby serve` serves tilesets with
//! a map of them, and `manby stats` reports their tiles' sizes.

use std::ffi::OsString;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc's settings: a few arenas, so that memory one thread frees is
/// reused by the others, and freed pages go back to the system at once, so
/// that the peak memory is the peak of what is in use.
#[repr(transparent)]
pub struct MallocConf(*const u8);
// SAFETY: the pointer is to a static string that is never written.
unsafe impl Sync for MallocConf {}
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MALLOC_CONF: MallocConf =
    MallocConf(c"narenas:4,dirty_decay_ms:0,muzzy_decay_ms:0,retain:true".as_ptr() as *const u8);

/// tippecanoe's programs that manby runs as tippecanoe-rs has them: the
/// subcommand, the program's name, and its `main`.
const TOOLS: [(&str, &str, fn()); 4] = [
    ("decode", "tippecanoe-decode", tippecanoe::decode::main),
    (
        "enumerate",
        "tippecanoe-enumerate",
        tippecanoe::enumerate::main,
    ),
    (
        "overzoom",
        "tippecanoe-overzoom",
        tippecanoe::overzoom::main,
    ),
    (
        "json-tool",
        "tippecanoe-json-tool",
        tippecanoe::jsontool::main,
    ),
];

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let name = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a))
        .unwrap_or("manby");

    if let Some(&(_, _, tool)) = TOOLS.iter().find(|t| t.1 == name) {
        return tool();
    }
    let subcommand = argv.get(1).map(String::as_str).unwrap_or("");
    if let Some(&(_, program, _)) = TOOLS
        .iter()
        .find(|t| t.0 == subcommand || t.1 == subcommand)
    {
        run_as(program, &argv[0]);
    }

    let result = if name == "tile-join" {
        // under tile-join's name, manby is tile-join
        let commandline = tippecanoe::text::format_commandline(&argv);
        join(manby::join::args::parse_args(&argv, commandline))
    } else if subcommand == "stats" {
        let mut stats_argv = vec![format!("{} stats", argv[0])];
        stats_argv.extend_from_slice(&argv[2..]);
        manby::stats::parse_args(&stats_argv)
            .and_then(|o| manby::stats::stats(&o))
            .map(|_| false)
    } else if subcommand == "serve" {
        let mut serve_argv = vec![format!("{} serve", argv[0])];
        serve_argv.extend_from_slice(&argv[2..]);
        manby::serve::parse_args(&serve_argv)
            .and_then(|o| manby::serve::serve(&o))
            .map(|_| false)
    } else if subcommand == "join" || subcommand == "tile-join" {
        let commandline = tippecanoe::text::format_commandline(&argv);
        let mut join_argv = vec![format!("{} {}", argv[0], subcommand)];
        join_argv.extend_from_slice(&argv[2..]);
        join(manby::join::args::parse_args(&join_argv, commandline))
    } else {
        let mut argv = argv.clone();
        if subcommand == "tippecanoe" {
            argv.remove(1);
        }
        manby::options::parse_args(&argv)
            .and_then(|options| manby::run(&options))
            .map(|summary| summary.incomplete)
    };
    match result {
        Ok(incomplete) => {
            if incomplete {
                std::process::exit(tippecanoe::errors::EXIT_INCOMPLETE);
            }
        }
        Err(e) => {
            // The messages are written out in full, as tippecanoe prints them.
            if !e.message.is_empty() {
                eprintln!("{}", e.message);
            }
            std::process::exit(e.exit_code);
        }
    }
}

fn join(options: manby::Result<manby::JoinOptions>) -> manby::Result<bool> {
    options.and_then(|o| manby::join(&o)).map(|_| false)
}

/// Runs this program again as `program`, with the arguments after the
/// subcommand. tippecanoe-rs's programs read the process's own arguments, so
/// they are given the arguments, and the name, they would have.
fn run_as(program: &str, argv0: &str) -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(2).collect();
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!(
            "{}: can't find this program to run {}: {}",
            argv0, program, e
        );
        std::process::exit(tippecanoe::errors::EXIT_OPEN);
    });
    let mut command = std::process::Command::new(exe);
    command.args(args);
    #[cfg(unix)]
    let err = {
        use std::os::unix::process::CommandExt;
        command.arg0(program).exec()
    };
    #[cfg(not(unix))]
    let err = std::io::Error::other(format!(
        "only under the name {} on this system, through a link for example",
        program
    ));
    eprintln!("{}: can't run {}: {}", argv0, program, err);
    std::process::exit(tippecanoe::errors::EXIT_OPEN);
}
