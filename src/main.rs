mod app;
mod create;
mod credentials;
mod dirty;
mod gitutil;
mod linear;
mod note_util;
mod persist;
mod settings;
mod switch;
mod task;
mod text_input;
mod treehouse;
mod ui;

use std::io::stdout;

use app::App;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = App::new()?.run(&mut terminal);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
