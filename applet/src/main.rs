// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod config;
mod dbus_client;
mod i18n;

fn main() -> cosmic::iced::Result {
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);
    cosmic::applet::run::<app::AppModel>(())
}
