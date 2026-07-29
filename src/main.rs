mod app;
mod create;
mod credentials;
mod dirty;
mod gitutil;
mod linear;
mod persist;
mod settings;
mod switch;
mod task;
mod text_input;
mod treehouse;
mod ui;

use app::App;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let mut terminal = ratatui::init();
    let result = App::new()?.run(&mut terminal);
    ratatui::restore();
    result
}
