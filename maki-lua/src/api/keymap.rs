use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{AppDataRefMut, Lua, RegistryKey, Result as LuaResult, Table};

use crate::api::util::convert::opt_bool;
use crate::api::util::pair::{Pair, pair};
use crate::key::Key;

static NEXT_KEYMAP_ID: AtomicU64 = AtomicU64::new(1);

const NO_STORE_ERR: &str = "keymap store not initialized";
const RESERVED_KEY_ERR: &str = "is reserved by the host and would never reach this binding";
const TAKEN_ERR: &str = "is already mapped by";

/// Keystrokes one plugin may have in flight before its bindings stop taking
/// keys. Each plugin is counted on its own, so one that parks in a callback
/// costs itself its keys and nobody else theirs.
const MAX_IN_FLIGHT: usize = 8;

/// What a key resolves to, handed to the Lua thread whole. Resolving by id
/// over there instead can find nothing, which leaves the UI having consumed a
/// key with nothing to act on it.
#[derive(Clone, Debug)]
struct Keybind {
    callback: Arc<RegistryKey>,
    /// Shared by every binding of one plugin.
    in_flight: Arc<AtomicUsize>,
    /// Shared by every binding of one plugin, and cleared when its load is
    /// torn down.
    live: Arc<AtomicBool>,
}

/// A keystroke on its way to the Lua thread, holding one slot of its plugin's
/// budget until the callback finishes.
pub struct KeybindTicket {
    callback: Arc<RegistryKey>,
    in_flight: Arc<AtomicUsize>,
    live: Arc<AtomicBool>,
    plugin: Arc<str>,
    key: Key,
}

impl KeybindTicket {
    /// Refuses the key when the plugin's load is gone or its budget is full.
    /// Both answers are the host's to act on in the same key turn: it runs the
    /// built-in binding instead, rather than handing the key to a callback
    /// that cannot answer it.
    fn claim(entry: &KeymapEntry, key: Key) -> Option<Self> {
        let bind = &entry.bind;
        if !bind.live.load(Ordering::Acquire) {
            return None;
        }
        bind.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_IN_FLIGHT).then_some(n + 1)
            })
            .ok()?;
        Some(Self {
            callback: Arc::clone(&bind.callback),
            in_flight: Arc::clone(&bind.in_flight),
            live: Arc::clone(&bind.live),
            plugin: Arc::clone(&entry.plugin),
            key,
        })
    }

    pub fn callback(&self) -> &RegistryKey {
        &self.callback
    }

    /// Whether the load that registered this binding is still the one in
    /// place. Read again on the Lua thread: a `/reload` can land between the
    /// claim and the call, and the plugin it tore down must run nothing.
    pub fn plugin_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// The load that registered the binding. An [`Arc`] rather than a borrow
    /// because the logs that name it run after the ticket has been handed on.
    pub fn plugin(&self) -> &Arc<str> {
        &self.plugin
    }

    /// The keystroke the host consumed to get here, for the log on the one
    /// path that loses it: a callback that cannot be reached at all.
    pub fn key(&self) -> Key {
        self.key
    }
}

impl Drop for KeybindTicket {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug)]
pub struct KeymapEntry {
    pub key: Key,
    pub desc: String,
    pub plugin: Arc<str>,
    pub id: u64,
    bind: Keybind,
}

#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    pub entries: Vec<KeymapEntry>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct KeymapReader(Arc<ArcSwap<KeymapSnapshot>>);

impl KeymapReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default())))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<KeymapSnapshot>> {
        self.0.load()
    }

    /// Hands {key} to the binding on it and reports whether it was taken.
    ///
    /// A key nothing took is reported untaken, and the host runs its own
    /// binding for it in the same key turn. That is the whole of fall-through:
    /// no key is ever handed back afterwards, because a keystroke replayed
    /// into a UI that has moved on lands somewhere the user never aimed it. A
    /// plugin with too many callbacks in flight, or one whose load is gone, is
    /// a plugin that took nothing, settled here before any Lua runs.
    ///
    /// These are global bindings, live until the plugin drops them. A key a
    /// popup should own only while it is on screen is not one of them: it is
    /// declared in the `keys` of `maki.ui.open_win`, and the host routes it to
    /// that window before it ever looks here.
    pub fn dispatch(&self, key: Key, run: impl FnOnce(KeybindTicket) -> bool) -> bool {
        let snapshot = self.0.load();
        let ticket = snapshot
            .entries
            .iter()
            .find(|e| e.key == key)
            .and_then(|entry| KeybindTicket::claim(entry, key));
        ticket.is_some_and(run)
    }
}

pub(crate) struct KeymapWriter {
    store: Arc<ArcSwap<KeymapSnapshot>>,
    generation: AtomicU64,
}

impl KeymapWriter {
    pub fn new() -> (Self, KeymapReader) {
        let inner = Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            KeymapReader(inner),
        )
    }

    pub fn publish(&self, entries: Vec<KeymapEntry>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(KeymapSnapshot {
            entries,
            generation,
        }));
    }
}

pub(crate) struct StoredKeymap {
    id: u64,
    key: Key,
    /// Dropping the last reference hands the registry slot back to mlua, which
    /// the next binding reuses, so a callback an in-flight keystroke still
    /// holds frees itself once that keystroke is done.
    callback: Arc<RegistryKey>,
    plugin: Arc<str>,
    desc: String,
    state: PluginDispatch,
}

/// What every binding of one plugin shares: the keystroke budget they are
/// counted against, and whether the load that registered them is still the one
/// in place.
#[derive(Clone)]
struct PluginDispatch {
    in_flight: Arc<AtomicUsize>,
    live: Arc<AtomicBool>,
}

impl Default for PluginDispatch {
    fn default() -> Self {
        Self {
            in_flight: Arc::default(),
            live: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// What a `set` did, and the one thing the caller has to do about it.
pub(crate) enum SetOutcome {
    /// Nothing else held the key.
    Free,
    /// Another owner held it. Its binding is underneath and comes back when
    /// this one is removed, so this is a warning naming both.
    Shadowed(Arc<str>),
    /// `unique` was asked for and the named owner already holds the key.
    /// Nothing was stored.
    Taken(Arc<str>),
    /// The load that called this is gone. Nothing was stored.
    Stale,
}

/// Global bindings, newest first, and that is the only ordering in this file:
/// [`Self::set`] inserts at the front, [`Self::snapshot_entries`] maps in
/// order, and [`KeymapReader::dispatch`] takes the first match.
pub(crate) struct KeymapStore {
    globals: Vec<StoredKeymap>,
    plugins: HashMap<Arc<str>, PluginDispatch>,
}

impl KeymapStore {
    pub fn new() -> Self {
        Self {
            globals: Vec::new(),
            plugins: HashMap::new(),
        }
    }

    /// The budget and the liveness every binding of {plugin} shares. A name
    /// [`Self::clear_plugin`] tombstoned gets that dead state back rather than
    /// a fresh live one: between a `/reload` and the load that replaces it, a
    /// handler of the load that is gone can still be running and still call
    /// `set`, and the `clear_plugin` that would have taken those bindings away
    /// has already run.
    fn plugin_state(&mut self, plugin: &Arc<str>) -> PluginDispatch {
        self.plugins.entry(Arc::clone(plugin)).or_default().clone()
    }

    /// Puts {plugin}'s binding for {key} on top of whatever else holds it.
    ///
    /// Bindings stack per owner: the caller's own entry for the key is
    /// replaced, everyone else's is shadowed and comes back when this one goes
    /// away. A plugin cannot hold another plugin's `RegistryKey`, so the
    /// save-and-restore vim leaves to `maparg()` has to be automatic here.
    ///
    /// With {unique}, any existing binding for the key fails the call,
    /// including the caller's own, which is what `:map <unique>` means. A
    /// reload cannot trip on that because [`Self::revive`] clears the old
    /// load's entries first.
    ///
    /// A `set` from a load that is gone stores nothing. Such a straggler used
    /// to publish a binding that could never fire; on a stack it would also
    /// sit on top of a live binding from another plugin and make that key fall
    /// through until the next reload.
    pub fn set(
        &mut self,
        key: Key,
        callback: RegistryKey,
        plugin: Arc<str>,
        desc: String,
        unique: bool,
    ) -> SetOutcome {
        let state = self.plugin_state(&plugin);
        if !state.live.load(Ordering::Acquire) {
            return SetOutcome::Stale;
        }
        if unique && let Some(owner) = self.owner_of(key) {
            return SetOutcome::Taken(owner);
        }
        self.globals.retain(|b| b.key != key || b.plugin != plugin);
        let shadowed = self.owner_of(key);
        self.globals.insert(
            0,
            StoredKeymap {
                id: NEXT_KEYMAP_ID.fetch_add(1, Ordering::Relaxed),
                key,
                callback: Arc::new(callback),
                plugin,
                desc,
                state,
            },
        );
        shadowed.map_or(SetOutcome::Free, SetOutcome::Shadowed)
    }

    /// Removes {plugin}'s binding for {key}, uncovering whatever it shadowed.
    ///
    /// Answers with the owner still holding the key when the caller had no
    /// binding of its own: that is a `del` aimed at somebody else's mapping,
    /// which changes nothing and is worth a warning.
    pub fn del(&mut self, key: Key, plugin: &str) -> Option<Arc<str>> {
        let mine = self
            .globals
            .iter()
            .position(|b| b.key == key && b.plugin.as_ref() == plugin);
        if let Some(idx) = mine {
            self.globals.remove(idx);
            return None;
        }
        self.owner_of(key)
    }

    /// The plugin whose binding for {key} would fire, which is the newest one.
    fn owner_of(&self, key: Key) -> Option<Arc<str>> {
        self.globals
            .iter()
            .find(|b| b.key == key)
            .map(|b| Arc::clone(&b.plugin))
    }

    /// The load is marked dead as well as emptied of keys. The snapshot loses
    /// its bindings either way, but a keystroke already on its way to the Lua
    /// thread carries its callback with it, and that callback belongs to the
    /// chunk this tore down.
    ///
    /// The name is tombstoned rather than forgotten. A keybind handler holds
    /// its budget slot for a bounded time and the drain barrier waits no
    /// longer, so a `/reload` can land while one is still running; that
    /// handler can call `set`, and a forgotten name would hand it a fresh live
    /// state and publish bindings for a load that is gone, which the
    /// `clear_plugin` that already ran will never clear. [`Self::revive`] is
    /// the one way back.
    pub fn clear_plugin(&mut self, plugin: &str) {
        self.globals.retain(|b| b.plugin.as_ref() != plugin);
        if let Some(state) = self.plugins.get(plugin) {
            state.live.store(false, Ordering::Release);
        }
    }

    /// Takes the tombstone off {plugin}, for the load that is about to
    /// register its bindings. Called before that load's chunks run and
    /// nowhere else, which is what tells a new load apart from a handler of
    /// the old one. Anything the old load's stragglers published in between
    /// goes with it: those bindings are dead and belong to nothing.
    pub fn revive(&mut self, plugin: &str) {
        self.globals.retain(|b| b.plugin.as_ref() != plugin);
        self.plugins.remove(plugin);
    }

    /// Every global binding, newest first, which is dispatch order and the
    /// order a keymap listing reads them in: the first entry per key is the
    /// one that fires.
    pub fn snapshot_entries(&self) -> Vec<KeymapEntry> {
        self.globals
            .iter()
            .map(|b| KeymapEntry {
                key: b.key,
                desc: b.desc.clone(),
                plugin: Arc::clone(&b.plugin),
                id: b.id,
                bind: Keybind {
                    callback: Arc::clone(&b.callback),
                    in_flight: Arc::clone(&b.state.in_flight),
                    live: Arc::clone(&b.state.live),
                },
            })
            .collect()
    }
}

/// The one gate a key a plugin names passes: canonical notation in, and the
/// keys the host answers itself refused. `maki.keymap.set` and the `keys` an
/// unfocused window claims both come through here, so a spelling that binds is
/// exactly a spelling that can be claimed.
pub(crate) fn accept_key(lhs: &str) -> LuaResult<Key> {
    let key = Key::parse(lhs).map_err(mlua::Error::runtime)?;
    reject_reserved(lhs, key)?;
    Ok(key)
}

fn publish_keymap_snapshot(lua: &Lua) {
    let Some(store) = lua.app_data_ref::<KeymapStore>() else {
        return;
    };
    let entries = store.snapshot_entries();
    drop(store);
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.publish(entries);
    }
}

/// Refuses a key the host answers itself, so a binding that could never fire
/// is an error the plugin author reads instead of a mapping that silently
/// never runs. One list, [`crate::key::RESERVED_KEYS`], answers this, the
/// `keys` a window claims, and the host.
pub(crate) fn reject_reserved(lhs: &str, key: Key) -> LuaResult<()> {
    if key.is_reserved() {
        return Err(mlua::Error::runtime(format!("{lhs} {RESERVED_KEY_ERR}")));
    }
    Ok(())
}

fn store_mut(lua: &Lua) -> LuaResult<AppDataRefMut<'_, KeymapStore>> {
    lua.app_data_mut::<KeymapStore>()
        .ok_or_else(|| mlua::Error::runtime(NO_STORE_ERR))
}

/// Bind a key to a Lua function, just like `vim.keymap.set`. Only
/// normal mode (`"n"`) is supported right now.
///
/// The binding belongs to the plugin that set it, and bindings stack: the
/// last `set` wins, and `del` or unloading that plugin gives the key back to
/// whoever held it before. Shadowing another plugin's binding is a warning
/// naming both. Setting a key you already hold replaces your own binding
/// rather than stacking on it.
///
/// The binding is global and lasts until `del` or the plugin unloads. For a
/// key a popup should own only while it is on screen, declare it in the
/// `keys` of `maki.ui.open_win` instead: the host routes it to that window
/// and hands it back when the window closes.
///
/// A handler that runs owns the key. Its return value is not read, and a
/// handler that raises is logged with the key spent all the same: a keystroke
/// replayed once the UI has moved on lands somewhere the user never aimed it.
///
/// A binding that cannot claim the key, because its plugin has too many
/// callbacks in flight, falls through to the host's own binding rather than
/// to the binding it shadows. The user aimed at the top one, and the host is
/// the honest fallback.
///
/// `<C-c>` and `<C-z>` are the two keys no binding takes: quitting and
/// suspending have to work whatever a plugin is doing. Binding one is an
/// error rather than a mapping that never fires.
///
/// @param mode string Mode letter. Currently only `"n"` is accepted.
/// @param lhs string Key in Vim notation, e.g. `"<C-t>"`, `"<Space>"`, `"a"`.
/// @param rhs function Called when the key is pressed. Its return value is not read.
/// @param opts table? Options:
///   `desc` (string) short description shown in the keymap list.
///   `unique` (boolean) fail the call, naming the owner, when anything already
///   maps the key. Default false.
/// @example
/// maki.keymap.set("n", "<C-t>", function()
///   print("toggle!")
/// end, { desc = "Toggle panel" })
#[lua_fn]
fn set(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    mode: String,
    lhs: String,
    rhs: mlua::Function,
    opts: Option<Table>,
) -> LuaResult<()> {
    if mode != "n" {
        return Err(mlua::Error::runtime(format!(
            "unsupported keymap mode: {mode}"
        )));
    }
    let key = accept_key(&lhs)?;
    let desc = opts
        .as_ref()
        .and_then(|o| o.get::<String>("desc").ok())
        .unwrap_or_default();
    let unique = opts
        .as_ref()
        .and_then(|o| opt_bool(o, "unique"))
        .unwrap_or(false);
    let registry_key = lua.create_registry_value(rhs)?;
    let outcome = store_mut(lua)?.set(key, registry_key, Arc::clone(&plugin), desc, unique);
    match outcome {
        SetOutcome::Free => {}
        SetOutcome::Shadowed(owner) => {
            tracing::warn!(key = %lhs, plugin = %plugin, shadowed = %owner, "keymap shadowed by plugin");
        }
        SetOutcome::Taken(owner) => {
            return Err(mlua::Error::runtime(format!("{lhs} {TAKEN_ERR} {owner}")));
        }
        SetOutcome::Stale => {
            tracing::debug!(key = %lhs, plugin = %plugin, "keymap dropped: plugin unloaded");
        }
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Remove your plugin's mapping for {lhs} in {mode}, which gives the key back
/// to whichever plugin held it before you. Does nothing if you have no
/// mapping for that key, and warns when another plugin holds it.
///
/// @param mode string Mode letter (reserved for future modes).
/// @param lhs string Key to unmap, in Vim notation.
/// @example
/// maki.keymap.del("n", "<C-t>")
#[lua_fn]
fn del(lua: &Lua, #[ctx] plugin: Arc<str>, mode: String, lhs: String) -> LuaResult<()> {
    let _ = mode;
    let key = Key::parse(&lhs).map_err(mlua::Error::runtime)?;
    if let Some(mut store) = lua.app_data_mut::<KeymapStore>()
        && let Some(owner) = store.del(key, &plugin)
    {
        tracing::warn!(key = %lhs, plugin = %plugin, owner = %owner, "keymap del left another plugin's binding alone");
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Canonical spelling of {lhs}, so no plugin has to know which of
/// `<CR>`/`<Enter>`/`<Return>` maki prints. Every spelling `set` accepts is
/// accepted here, and the answer is the string a `key` event carries.
///
/// @param lhs string Key in any accepted notation.
/// @return (string|nil, string|nil) Canonical notation, or nil and an error.
/// @example
/// local canon = maki.keymap.normalize("<Enter>")  -- "<CR>"
#[lua_fn]
fn normalize(_lua: &Lua, lhs: String) -> LuaResult<Pair<String>> {
    Ok(pair(Key::parse(&lhs).map(|key| key.notation())))
}

lua_table! {
    /// Key mappings, modeled after `vim.keymap`. If you have written a
    /// Neovim keymap plugin before, this will feel familiar.
    ///
    /// `set` claims a key for the rest of the run. A key a popup should own
    /// only while it is on screen belongs in the `keys` of
    /// `maki.ui.open_win`, which routes it to that window and hands it back
    /// when the window closes.
    ///
    /// ```lua
    /// maki.keymap.set("n", "<C-t>", function()
    ///   print("hello")
    /// end, { desc = "Say hello" })
    /// ```
    ///
    /// ## Key notation
    ///
    /// One notation covers every place maki names a key: the string `set` and
    /// `del` read, the `keys` a window claims in `maki.ui.open_win`, and the
    /// `key` field of a `win:recv` keypress event. `normalize` turns any
    /// accepted spelling into the one maki prints.
    ///
    /// A single character stands for itself: `a`, `A`, `7`, `?`. Every other
    /// key goes in angle brackets, behind its modifier prefixes.
    ///
    /// | Key | Notation | Also accepted |
    /// | --- | --- | --- |
    /// | Enter | `<CR>` | `<Enter>`, `<Return>` |
    /// | Escape | `<Esc>` | `<Escape>` |
    /// | Backspace | `<BS>` | `<Backspace>` |
    /// | Delete | `<Del>` | `<Delete>` |
    /// | Tab | `<Tab>` | |
    /// | Shift+Tab | `<S-Tab>` | |
    /// | Space | `<Space>` | |
    /// | Arrows | `<Up>`, `<Down>`, `<Left>`, `<Right>` | |
    /// | Navigation | `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Insert>` | |
    /// | Function keys | `<F1>` through `<F24>` | |
    ///
    /// Modifiers are `C-` for control, `M-` for alt and `S-` for shift,
    /// written in that order when a key carries more than one: `<C-M-x>`.
    /// `Ctrl-`, `Alt-`, `A-` and `Shift-` are read on the way in and never
    /// printed.
    ///
    /// Terminals disagree with each other about three keys, so maki settles
    /// each one way:
    ///
    /// - Control plus a letter is lowercase, so `<C-N>` is `<C-n>`. Vim has
    ///   the same rule, and crossterm gives maki the real case to apply it to.
    /// - Shift plus a letter is the uppercase letter, so `<S-a>` is `A`. Shift
    ///   plus a digit or a punctuation mark keeps its prefix: `<S-1>`.
    /// - Shift+Tab is `<S-Tab>` whether or not the terminal speaks the kitty
    ///   keyboard protocol.
    ///
    /// Write the notation itself; there is nothing to import. A key spelling
    /// in a plugin's source is checked when the plugin loads, and one that is
    /// wrong is reported on screen naming the file and line, so a typo is a
    /// message at startup rather than a binding that quietly never fires.
    ///
    /// ```lua
    /// if ev.type == "key" and ev.key == "<CR>" then submit() end
    /// ```
    ///
    /// Earlier versions of maki delivered a `win:recv` key event as `"enter"`,
    /// `"esc"`, `"ctrl+n"` or `"shift+tab"`. The same presses now arrive as
    /// `<CR>`, `<Esc>`, `<C-n>` and `<S-Tab>`. That check reports the old
    /// spellings by name, so a plugin written against them says so at startup
    /// instead of going quiet.
    "maki.keymap" => pub(crate) fn create_keymap_table(plugin: Arc<str>), DOCS [
        set(plugin), del(plugin), normalize,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PLUGIN: &str = "plug";
    const OTHER_PLUGIN: &str = "other";
    const TAB: &str = "<Tab>";
    const ESC: &str = "<Esc>";

    fn parsed(lhs: &str) -> Key {
        Key::parse(lhs).unwrap()
    }

    fn global(store: &mut KeymapStore, lua: &Lua, lhs: &str, plugin: &str) -> SetOutcome {
        bind(store, lua, lhs, plugin, false)
    }

    fn bind(
        store: &mut KeymapStore,
        lua: &Lua,
        lhs: &str,
        plugin: &str,
        unique: bool,
    ) -> SetOutcome {
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let k = lua.create_registry_value(f).unwrap();
        store.set(parsed(lhs), k, Arc::from(plugin), String::new(), unique)
    }

    fn published(store: &KeymapStore) -> KeymapReader {
        let (writer, reader) = KeymapWriter::new();
        writer.publish(store.snapshot_entries());
        reader
    }

    /// Which plugin's binding for {lhs} fires.
    fn dispatching_owner(store: &KeymapStore, lhs: &str) -> Option<Arc<str>> {
        let mut owner = None;
        published(store).dispatch(parsed(lhs), |ticket| {
            owner = Some(Arc::clone(ticket.plugin()));
            true
        });
        owner
    }

    /// Two plugins wanting one key is the ordinary case, and the loser has to
    /// get its key back rather than lose it for the rest of the run.
    #[test]
    fn a_shadowed_binding_comes_back_when_the_one_over_it_goes_away() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, TAB, OTHER_PLUGIN);
        assert_eq!(store.globals.len(), 2, "both bindings are kept");
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN),
            "the newest binding is the one that fires"
        );

        store.del(parsed(TAB), OTHER_PLUGIN);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(PLUGIN),
            "removing it uncovers the binding underneath"
        );

        global(&mut store, &lua, TAB, OTHER_PLUGIN);
        store.clear_plugin(OTHER_PLUGIN);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(PLUGIN),
            "unloading the owner uncovers it the same way"
        );
    }

    /// Rebinding your own key replaces your entry. Stacking it on yourself
    /// would grow a pile only a reload could clear.
    #[test]
    fn one_owner_setting_the_same_key_twice_keeps_one_entry() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert_eq!(store.globals.len(), 1);
    }

    /// `del` is scoped to the caller, so a plugin cannot unbind a key it never
    /// bound and leave the user with a dead keystroke.
    #[test]
    fn del_by_a_non_owner_leaves_the_binding_alone() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        assert_eq!(
            store.del(parsed(TAB), OTHER_PLUGIN).as_deref(),
            Some(PLUGIN),
            "the answer names the owner, for the warning"
        );
        assert_eq!(dispatching_owner(&store, TAB).as_deref(), Some(PLUGIN));
    }

    #[test]
    fn keymap_store_del() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, "x", PLUGIN);
        assert!(store.del(parsed("x"), PLUGIN).is_none());
        assert!(store.globals.is_empty());
    }

    /// `unique` is how a plugin says the key is no good to it shared. Vim
    /// fails the same way, and the error has to name who to go and look at.
    #[test]
    fn unique_refuses_a_key_anything_already_holds() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        assert!(matches!(
            bind(&mut store, &lua, TAB, PLUGIN, true),
            SetOutcome::Free
        ));
        let taken = bind(&mut store, &lua, TAB, OTHER_PLUGIN, true);
        assert!(
            matches!(&taken, SetOutcome::Taken(owner) if owner.as_ref() == PLUGIN),
            "the owner has to be named"
        );
        assert_eq!(store.globals.len(), 1, "nothing was stored");
    }

    #[test]
    fn keymap_store_clear_plugin() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, "t", PLUGIN);
        global(&mut store, &lua, "x", OTHER_PLUGIN);

        store.clear_plugin(PLUGIN);
        assert_eq!(store.globals.len(), 1);
        assert_eq!(store.globals[0].plugin.as_ref(), OTHER_PLUGIN);
    }

    #[test]
    fn snapshot_reader_writer() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        let (writer, reader) = KeymapWriter::new();
        assert!(reader.load().entries.is_empty());

        global(&mut store, &lua, TAB, PLUGIN);
        writer.publish(store.snapshot_entries());

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }

    /// The key is consumed the moment `dispatch` says it was claimed, so a
    /// hand-off that did not happen has to report so and let the built-in
    /// binding run.
    #[test_case(true  => true  ; "claimed_when_the_callback_was_reached")]
    #[test_case(false => false ; "falls_through_when_it_was_not")]
    fn dispatch_reports_whether_the_key_was_claimed(handed_off: bool) -> bool {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        published(&store).dispatch(parsed(TAB), |_| handed_off)
    }

    #[test]
    fn dispatch_leaves_a_key_nobody_claimed_alone() {
        let store = KeymapStore::new();
        assert!(!published(&store).dispatch(parsed(TAB), |_| unreachable!()));
    }

    /// A callback that parks holds its ticket, and the plugin that owns it
    /// runs out of budget. Its keys then reach the built-in binding, and every
    /// other plugin keeps dispatching.
    #[test]
    fn a_parked_plugin_stops_claiming_keys_and_leaves_the_others_alone() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, ESC, OTHER_PLUGIN);
        let reader = published(&store);

        let parked = exhaust(&reader, TAB);

        assert!(
            !reader.dispatch(parsed(TAB), |_| unreachable!()),
            "the parked plugin is out of budget"
        );
        assert!(
            reader.dispatch(parsed(ESC), |_| true),
            "another plugin's keys still dispatch"
        );

        drop(parked);
        assert!(
            reader.dispatch(parsed(TAB), |_| true),
            "finishing the callbacks gives the budget back"
        );
    }

    /// Fills {plugin}'s budget and returns the tickets holding it.
    fn exhaust(reader: &KeymapReader, lhs: &str) -> Vec<KeybindTicket> {
        (0..MAX_IN_FLIGHT)
            .map(|_| {
                let mut held = None;
                assert!(reader.dispatch(parsed(lhs), |t| {
                    held = Some(t);
                    true
                }));
                held.unwrap()
            })
            .collect()
    }

    /// A binding that cannot run its callback is a binding that is not there,
    /// and the host runs its own in the same keystroke. Swallowing the key
    /// instead leaves the user pressing a key nothing will answer.
    #[test]
    fn an_exhausted_binding_falls_through() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        let reader = published(&store);

        let _parked = exhaust(&reader, TAB);

        assert!(!reader.dispatch(parsed(TAB), |_| unreachable!()));
    }

    /// A keystroke claimed a moment before a `/reload` carries the old chunk's
    /// callback with it. Its plugin is gone, so the host has to be told here,
    /// while it can still run the built-in binding for the key.
    #[test]
    fn a_binding_whose_plugin_was_torn_down_claims_nothing() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        let reader = published(&store);

        let mut live = None;
        assert!(reader.dispatch(parsed(TAB), |t| {
            live = Some(t.plugin_live());
            true
        }));
        assert_eq!(live, Some(true));

        store.clear_plugin(PLUGIN);
        assert!(
            !reader.dispatch(parsed(TAB), |_| unreachable!()),
            "the stale snapshot no longer claims the key"
        );
    }

    /// A ticket outlives the snapshot it came from, so the answer has to travel
    /// with it: a `/reload` between the claim and the call must reach the
    /// callback that is about to run.
    #[test]
    fn a_ticket_reports_the_teardown_that_landed_after_it_was_claimed() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut held = None;
        published(&store).dispatch(parsed(TAB), |t| {
            held = Some(t);
            true
        });
        let ticket = held.unwrap();
        assert!(ticket.plugin_live());
        assert_eq!(ticket.plugin().as_ref(), PLUGIN);

        store.clear_plugin(PLUGIN);
        assert!(!ticket.plugin_live());
    }

    /// The handler runs on another thread, so the key the host consumed travels
    /// with the ticket: it is what the log names when the callback cannot be
    /// reached at all.
    #[test]
    fn a_ticket_carries_the_key_it_was_claimed_for() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut carried = None;
        published(&store).dispatch(parsed(TAB), |t| {
            carried = Some(t.key());
            true
        });
        assert_eq!(carried, Some(parsed(TAB)));
    }

    /// A reserved key binds nowhere. Accepting it publishes a binding with a
    /// `desc` in the keymap list that can never fire, which is a bug report
    /// about maki rather than about the plugin that wrote it.
    #[test_case("<C-c>" ; "quit")]
    #[test_case("<C-z>" ; "suspend")]
    fn a_reserved_key_is_refused_where_the_author_can_see_it(lhs: &str) {
        let err = reject_reserved(lhs, parsed(lhs)).unwrap_err().to_string();
        assert!(err.contains(RESERVED_KEY_ERR), "got: {err}");
        assert!(
            err.contains(lhs),
            "the error has to name the key, got: {err}"
        );
    }

    /// A tombstone, not a removal: a handler of the load that is gone can
    /// still be running when `/reload` lands, and the bindings it publishes on
    /// its way out belong to nothing. The load that replaces it starts live.
    #[test]
    fn a_torn_down_plugin_cannot_publish_live_bindings_until_it_is_reloaded() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        store.clear_plugin(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            !published(&store).dispatch(parsed(TAB), |_| unreachable!()),
            "a straggler of the torn down load publishes nothing that fires"
        );

        store.revive(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            published(&store).dispatch(parsed(TAB), |_| true),
            "the load that replaced it dispatches"
        );
    }

    /// The straggler's `set` stores nothing, which is what keeps a live
    /// binding from another plugin from being buried under a dead one until
    /// the next reload.
    #[test]
    fn a_straggler_does_not_bury_a_live_binding_under_a_dead_one() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        store.clear_plugin(PLUGIN);
        global(&mut store, &lua, TAB, OTHER_PLUGIN);

        assert!(matches!(
            global(&mut store, &lua, TAB, PLUGIN),
            SetOutcome::Stale
        ));
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN),
            "the live binding still fires"
        );
    }
}
