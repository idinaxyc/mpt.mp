mod cover;
mod engine;
mod iced_ui;
mod library;
mod model;
mod player;
mod plugins;
mod settings;
mod theme;
mod transfer;

fn main() -> iced::Result {
    model::ensure_dirs();
    theme::init();
    iced_ui::run()
}
