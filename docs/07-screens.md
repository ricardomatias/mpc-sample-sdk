# Screens and page detection

The app is one window with several pages. The SDK tracks the component that is
currently painting and gives you the same checks the app uses.

## The basics

```rust
use mpc_sample_sdk::platform::page;

let screen = page::screen();          // the current component
let is_sample = page::is_sample_page(screen);
// SAFETY: `screen` is the live page recorded by the paint hook on this thread.
let visible = unsafe { page::visible(screen) };
// SAFETY: as above.
let chopping = unsafe { page::chop_mode(screen) };
```

`page::screen()` is updated by the paint hook. It is zero before the first paint.
Use `visible` before reading selection data: it checks the vtable **and** the
app's “this page has a sample” predicate.

## Example: paint only on the Sample page

```rust
use mpc_sample_sdk::{draw::Graphics, Mod, PaintEvent};

#[derive(Default)]
struct ScreenAware;

impl Mod for ScreenAware {
    fn on_paint(&mut self, graphics: &mut Graphics, event: PaintEvent) {
        if !event.sample_page {
            return;
        }
        if event.chop_mode {
            // SAFETY: the app is painting this live component on the UI thread.
            unsafe {
                graphics.colour(0xffff_cc00);
                graphics.rect(4, 4, 120, 18);
                graphics.colour(0xff00_0000);
                graphics.text("CHOP", 10, 6, 100, 14, 12.0);
            }
        }
    }
}

mpc_sample_sdk::export_mod!(ScreenAware);
```

`PaintEvent.sample_page` is the cheap check inside the paint callback.
`unsafe { page::visible(page::screen()) }` is the equivalent outside it, with
the same live-page contract.

## Example: react to a page change

The SDK does not send a “page changed” event. Compare the last value in your
paint callback:

```rust
use mpc_sample_sdk::{draw::Graphics, Mod, PaintEvent};

#[derive(Default)]
struct PageChange {
    last: usize,
}

impl Mod for PageChange {
    fn on_paint(&mut self, _graphics: &mut Graphics, event: PaintEvent) {
        if event.sample_page && event.component != self.last {
            self.last = event.component;
            mpc_sample_sdk::draw::show_toast("Sample page");
        }
    }
}
```

## Screen-related constants

```rust
use mpc_sample_sdk::platform::page;

assert_eq!(page::SAMPLE_PAGE_VTABLE, 0x6e2_12a0);
let screen = page::screen();
if page::is_sample_page(screen) {
    mpc_sample_sdk::platform::status::set_status("on the Sample page");
}
```

The vtable constant is firmware-specific. If you support another firmware
version, verify it before using the page helpers.

## Run a stock SHIFT + pad shortcut

With SHIFT held, each pad opens a stock page (PAD 8 is MIDI Config, PAD 16 the
Project page). A mod that takes one of those chords for itself can still open
the stock page:

```rust
use mpc_sample_sdk::platform::shortcut;

if !shortcut::open_midi_settings() {
    // The app has not built its control-surface behaviour yet.
}
// Any other pad, from zero: PAD 16 is 15.
let _ = shortcut::shift_pad(15);
```

This runs the app's own SHIFT + pad handler, so it behaves exactly like the
chord: it is ignored where the chord would be, and the page change is queued to
the UI thread rather than made during the call. `shortcut::ready()` is false
until the app has built the object, which happens after `on_start`.

## The Sample Browser

`platform::browser` reads the stock Sample Browser (SAMPLE SELECT) and lets a
mod change what it lists:

```rust
use mpc_sample_sdk::platform::browser::{self, Kind, Listing};
use mpc_sample_sdk::{ButtonEvent, ButtonFlow, Mod};
use std::path::PathBuf;

#[derive(Default)]
struct Flat {
    on: bool,
}

impl Mod for Flat {
    fn on_browser_list(&mut self, listing: &mut Listing) {
        // The project browser uses the same classes: leave it alone.
        if self.on && listing.kind() == Some(Kind::Samples) {
            listing.replace(vec![
                PathBuf::from("/data/UserContent/Samples/User/Kicks/Kick 1.wav"),
                PathBuf::from("/media/MPC SAMPLE/MPC-Sample/Samples/Kick 2.wav"),
            ]);
        }
    }

    fn on_button(&mut self, event: ButtonEvent) -> ButtonFlow {
        if event.code == 54 && event.pressed && browser::is_open() {
            self.on = !self.on;
            // Posted to the UI thread; on_browser_list runs again there.
            let _ = browser::refresh();
            return ButtonFlow::Consume;
        }
        ButtonFlow::Forward
    }
}
```

- `browser::is_open()`: the browser screen is visible and listing samples.
- `browser::state()`: `Some(State { folder, location, highlighted })` while it
  is open; `folder()`, `location()` (`Internal` or `External`, the SCREEN 2
  tab), `highlighted()` and `cursor_row()` read the parts.
- `Mod::on_browser_list` runs on the UI thread every time a folder is listed.
  `Listing::replace` may list files from any folder or location, in any order;
  preview (SCREEN 3) and load (push DATA) follow the replaced rows, because the
  SDK also answers the list box's row lookup.
- `browser::refresh()` asks for a new listing from any callback. Each location
  has its own listing: after SCREEN 2 switches location, refresh again.
- Preview is the stock auto-preview: moving the cursor plays the highlighted
  file, and SCREEN 3 switches auto-preview on and off.

`platform::ui::post(job)` runs a `fn()` on the UI thread through the same queue
the stock SHIFT + pad shortcuts use; `refresh` is built on it.
