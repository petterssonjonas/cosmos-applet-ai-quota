// SPDX-License-Identifier: MPL-2.0

mod app;
mod auth;
mod model;
mod oauth;
mod providers;

fn main() -> cosmic::iced::Result {
    cosmic::applet::run::<app::AppModel>(())
}
