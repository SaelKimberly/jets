use clap::Parser;
use jets::app::cli::{Args, get_version};
use jets::app::env_vars::RESOURCES_DIR;
use jets::app::{App, Config};
use std::env;
use std::io::Result;

fn main() -> Result<()> {
    // mainly used for the github workflow
    let args: Vec<String> = env::args().collect();
    if args.len() == 2 && args[1] == "-v" {
        println!("{}", get_version());
        return Ok(());
    }

    let cwd = env::current_dir()?;
    if env::var(RESOURCES_DIR).is_err() {
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { env::set_var(RESOURCES_DIR, cwd.to_str().unwrap()) };
    }
    let args = Args::parse();
    let config = Config::load(args.get_config()?)?;
    App::run(config)
}
