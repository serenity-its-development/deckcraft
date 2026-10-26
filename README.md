# Slides

Slides is OneOS's fork of [DeckCraft](https://github.com/storytold/deckcraft): a presentation editor written in Rust, built for the
browser (WebAssembly) and for the desktop. OneOS hosts it through its Craft host bridge: the file is opened and saved by the host,
and a presentation can be edited live by several people through the host's collaboration room.

- `apps/deckcraft-web` - the browser build (`host.rs` is the host bridge, `collab.rs` the live editing wiring).
- `crates/collab` - the live editing shim: the deck as the host's shared operations (slides, shapes, text with formatting, notes,
  comments, pictures by content address). `crates/collab-node` builds it for node so OneOS can test two windows converging
  through its real CRDT engine.
- `crates/ui-egui/src/host_commands.rs` - the assistant's allow-listed commands.

MIT OR Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`. The upstream copyright notice is in `NOTICE`.
