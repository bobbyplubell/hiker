# Notes Query plugin

The dataview-style example plugin for hiker's WASM plugin system: a sidebar
panel with a query box and a live results table over the vault's structured
metadata index. It requests only `read:notes`, `read:metadata`, and a sidebar
panel — no write, no network.

This crate is excluded from the host workspace (it compiles to wasm32, and its
`init` / `on_ui_event` import `hiker.host_call`). Its pure logic
(`build_query_args`, `build_vdom`, `event_value`) is unit-tested on the host
target; the wasm ABI shim is `#[cfg(target_arch = "wasm32")]`.

## Test the logic (host target)

```sh
cargo test            # run from this directory
```

## Build the wasm artifact

```sh
rustup target add wasm32-unknown-unknown          # once
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/hiker_plugin_notes_query.wasm
```

## Install into a vault

1. Copy `manifest.json` + the built `.wasm` (as `plugin.wasm`) into
   `<vault>/.hiker/plugins/notes-query/`.
2. Compute the two blake3 pins and add an entry to
   `<vault>/.hiker/plugins.json` (see `docs/plugins.md`):

   ```json
   {
     "plugins": [
       {
         "id": "notes-query",
         "location": ".hiker/plugins/notes-query",
         "manifest_hash": "blake3:<hash of manifest.json>",
         "wasm_hash": "blake3:<hash of plugin.wasm>",
         "enabled": true
       }
     ]
   }
   ```

On next vault open the host verifies both pins, loads the plugin, and renders
its panel in the Plugins tab. A pin mismatch (the files changed on disk) aborts
the load rather than running changed code.

## Query syntax

Space-separated `key:value` tokens. `tag:project` matches the `tags` list;
other keys match a frontmatter field exactly (`status:active`). Non-`tag` keys
also become table columns. An empty query lists all notes, newest first.
