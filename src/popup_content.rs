use cocoa::appkit::{NSPanel, NSTextField};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};

#[derive(Copy, Clone)]
pub struct PopupContent {
    pub textfield: id,
    string: &'static str,
}

/// TODO: set image
/// https://github.com/tauri-apps/tray-icon/blob/b4fc8f888a07cb66661cf15d0da9d39951995e04/src/platform_impl/macos/mod.rs#L155
impl PopupContent {
    pub fn new(string: &'static str) -> Self {
        let textfield = unsafe {
            let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(200., 40.));
            let textfield = NSTextField::alloc(nil).initWithFrame_(rect);
            let _: () = msg_send![textfield, retain];
            let text = NSString::alloc(nil).init_str(string);
            // textfield.setStringValue_(NSString::alloc(nil).init_str(string));

            let _: () = msg_send![textfield, setBezeled: NO];
            let _: () = msg_send![textfield, setEditable: YES];
            // let _: () = msg_send![textfield, setDrawsBackground: NO];
            // let _: () = msg_send![textfield, setSelectable: NO];
            let _: () = msg_send![textfield, setStringValue: text];

            textfield
        };

        Self { textfield, string }
    }

    pub fn set_text(&mut self, string: &'static str) {
        self.string = string;
        unsafe {
            let text = NSString::alloc(nil).init_str(&self.string);
            self.textfield.setStringValue_(text);

            let _: () = msg_send![self.textfield, setStringValue: text];
        };
    }
}
