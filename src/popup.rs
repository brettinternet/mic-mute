use cacao::appkit::window::Window;
use cacao::appkit::{App, AppDelegate};

#[derive(Default)]
struct Popup {
    window: Window,
}

// https://github.com/ryanmcgrath/cacao
// https://github.com/kattrali/rust-mac-app-examples/blob/master/2-displaying-cocoa-window/src/main.rs

impl AppDelegate for Popup {
    fn did_finish_launching(&self) {
        self.window.set_minimum_content_size(400., 400.);
        self.window.set_title("Hello World!");
        self.window.show();
    }
}

fn main() {
    App::new("com.hello.world", Popup::default()).run();
}
