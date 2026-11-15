//! The application menus, in the place the platform keeps them.
//!
//! On macOS the menu bar belongs to the screen, not to the window: a Mac app
//! that draws Edit/View/Window/Help inside its own frame looks like a port of
//! something else, and the system menu bar sits above it saying only the app's
//! name. egui cannot draw there, because that bar is `NSMenu` and belongs to
//! `NSApplication`, so the native menu is built with `muda` and the in-window
//! menu bar is simply not drawn.
//!
//! Everywhere else the in-window bar stays, which is why [`NativeMenu::install`]
//! returns an `Option`: a `None` means "draw your own".
//!
//! The two paths share [`crate::app::MenuAction`], so a menu item means the same
//! thing whichever bar it was clicked in, and the logic behind it is tested once.

use crate::app::Tab;
use crate::theme::ThemeMode;

/// Everything the menus need to know about the app to show themselves
/// correctly: what is greyed out, what is ticked.
///
/// It is `Copy` and `PartialEq` on purpose. The app hands one over every frame
/// and the menu only touches the platform when it differs from the last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MenuState {
    /// A removal has happened and can be put back.
    pub can_undo: bool,
    /// Nothing is scanning or purging, so an action may start.
    pub can_act: bool,
    /// The server table is the visible tab; selection and search apply to it.
    pub on_servers: bool,
    pub has_checked: bool,
    pub has_search: bool,
    pub theme: ThemeMode,
    pub tab: Tab,
    pub show_disabled: bool,
    pub reveal_env: bool,
    pub sweep: bool,
    pub measure: bool,
}

pub use imp::NativeMenu;

#[cfg(target_os = "macos")]
mod imp {
    use super::MenuState;
    use crate::app::{MenuAction, Tab};
    use crate::theme::ThemeMode;
    use eframe::egui;
    use muda::accelerator::Accelerator;
    use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use std::cell::Cell;
    use std::str::FromStr;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};

    /// Ids are strings because that is what `muda` carries back in its events.
    /// They are matched in one place, [`action_for`], so an id that is created
    /// without a home shows up as a dead menu item rather than a silent one.
    mod ids {
        pub const UNDO: &str = "edit.undo";
        pub const SELECT_ALL: &str = "edit.select-all";
        pub const SELECT_NONE: &str = "edit.select-none";
        pub const FIND: &str = "edit.find";
        pub const CLEAR_SEARCH: &str = "edit.clear-search";
        pub const TAB_SERVERS: &str = "view.tab.servers";
        pub const TAB_ORPHANS: &str = "view.tab.orphans";
        pub const TAB_COVERAGE: &str = "view.tab.coverage";
        pub const THEME_SYSTEM: &str = "view.theme.system";
        pub const THEME_LIGHT: &str = "view.theme.light";
        pub const THEME_DARK: &str = "view.theme.dark";
        pub const SHOW_DISABLED: &str = "view.show-disabled";
        pub const REVEAL_ENV: &str = "view.reveal-env";
        pub const SWEEP: &str = "view.sweep";
        pub const MEASURE: &str = "view.measure";
        pub const RESCAN: &str = "view.rescan";
        pub const BACKUPS: &str = "help.backups";
        pub const REGISTRY: &str = "help.registry";
        pub const PROJECT: &str = "help.project";
        pub const ABOUT: &str = "app.about";
    }

    /// What a clicked item means. Unknown ids are `None` rather than a panic: a
    /// menu event can only arrive from a menu this module built, but the mapping
    /// is data and an unmapped id should do nothing, not kill the app.
    fn action_for(id: &str) -> Option<MenuAction> {
        Some(match id {
            ids::UNDO => MenuAction::Undo,
            ids::SELECT_ALL => MenuAction::SelectAll,
            ids::SELECT_NONE => MenuAction::SelectNone,
            ids::FIND => MenuAction::Find,
            ids::CLEAR_SEARCH => MenuAction::ClearSearch,
            ids::TAB_SERVERS => MenuAction::ShowTab(Tab::Servers),
            ids::TAB_ORPHANS => MenuAction::ShowTab(Tab::Orphans),
            ids::TAB_COVERAGE => MenuAction::ShowTab(Tab::Coverage),
            ids::THEME_SYSTEM => MenuAction::Theme(ThemeMode::System),
            ids::THEME_LIGHT => MenuAction::Theme(ThemeMode::Light),
            ids::THEME_DARK => MenuAction::Theme(ThemeMode::Dark),
            ids::SHOW_DISABLED => MenuAction::ToggleDisabled,
            ids::REVEAL_ENV => MenuAction::ToggleRevealEnv,
            ids::SWEEP => MenuAction::ToggleSweep,
            ids::MEASURE => MenuAction::ToggleMeasure,
            ids::RESCAN => MenuAction::Rescan,
            ids::BACKUPS => MenuAction::OpenBackups,
            ids::REGISTRY => MenuAction::ShowRegistryPath,
            ids::PROJECT => MenuAction::ProjectPage,
            ids::ABOUT => MenuAction::About,
            _ => return None,
        })
    }

    /// Accelerators are written as strings, which can fail to parse. They are
    /// all literals checked by a test below, so a failure here is a typo that
    /// has already been caught; at runtime it costs the shortcut, not the item.
    fn accel(s: &str) -> Option<Accelerator> {
        Accelerator::from_str(s).ok()
    }

    fn item(id: &str, text: &str, shortcut: Option<&str>) -> MenuItem {
        MenuItem::with_id(id, text, true, shortcut.and_then(accel))
    }

    fn check(id: &str, text: &str, shortcut: Option<&str>) -> CheckMenuItem {
        CheckMenuItem::with_id(id, text, true, false, shortcut.and_then(accel))
    }

    /// The real macOS menu bar.
    ///
    /// The items are kept rather than dropped after assembly because ticking
    /// and greying them is how the menu reports the app's state, and `muda`
    /// does that through the item handles.
    pub struct NativeMenu {
        /// Owns the `NSMenu`. Dropping it would take the menu bar with it.
        _menu: Menu,
        undo: MenuItem,
        select_all: MenuItem,
        select_none: MenuItem,
        find: MenuItem,
        clear_search: MenuItem,
        rescan: MenuItem,
        tabs: Vec<(Tab, CheckMenuItem)>,
        themes: Vec<(ThemeMode, CheckMenuItem)>,
        show_disabled: CheckMenuItem,
        reveal_env: CheckMenuItem,
        sweep: CheckMenuItem,
        measure: CheckMenuItem,
        /// The last state pushed to the platform, so an unchanged frame costs
        /// nothing. `Cell` because syncing is logically read-only to the app.
        last: Cell<Option<MenuState>>,
        events: Receiver<MenuEvent>,
    }

    impl NativeMenu {
        /// Build the menu bar and hand it to `NSApplication`.
        ///
        /// Must be called on the main thread, after the window exists, which is
        /// exactly where eframe runs the app's constructor.
        pub fn install(ctx: &egui::Context) -> Option<Self> {
            let about = item(ids::ABOUT, "About mcpurge", None);
            let app_menu = Submenu::with_items(
                "mcpurge",
                true,
                &[
                    &about,
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::services(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::hide(None),
                    &PredefinedMenuItem::hide_others(None),
                    &PredefinedMenuItem::show_all(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::quit(None),
                ],
            )
            .ok()?;

            // No shortcuts on these three. A menu accelerator is handled by
            // the system before the window sees the key, so Cmd+Z here would
            // undo a *removal* while the pointer sits in the search box
            // expecting to undo a word, and Cmd+A would select every server
            // instead of the text being typed. Neither is worth a shortcut.
            let undo = item(ids::UNDO, "Undo last removal", None);
            // "Select all" on a Mac means the text in a field, so these say
            // what they actually take: the rows the filter is showing.
            let select_all = item(ids::SELECT_ALL, "Select all servers", None);
            let select_none = item(ids::SELECT_NONE, "Deselect all", None);
            let find = item(ids::FIND, "Find\u{2026}", Some("CmdOrCtrl+F"));
            let clear_search = item(ids::CLEAR_SEARCH, "Clear search", None);
            let edit_menu = Submenu::with_items(
                "Edit",
                true,
                &[
                    &undo,
                    &PredefinedMenuItem::separator(),
                    &select_all,
                    &select_none,
                    &PredefinedMenuItem::separator(),
                    &find,
                    &clear_search,
                ],
            )
            .ok()?;

            let tabs = vec![
                (
                    Tab::Servers,
                    check(ids::TAB_SERVERS, "Servers", Some("CmdOrCtrl+1")),
                ),
                (
                    Tab::Orphans,
                    check(ids::TAB_ORPHANS, "Orphans", Some("CmdOrCtrl+2")),
                ),
                (
                    Tab::Coverage,
                    check(ids::TAB_COVERAGE, "Coverage", Some("CmdOrCtrl+3")),
                ),
            ];
            let themes = vec![
                (
                    ThemeMode::System,
                    check(ids::THEME_SYSTEM, ThemeMode::System.label(), None),
                ),
                (
                    ThemeMode::Light,
                    check(ids::THEME_LIGHT, ThemeMode::Light.label(), None),
                ),
                (
                    ThemeMode::Dark,
                    check(ids::THEME_DARK, ThemeMode::Dark.label(), None),
                ),
            ];
            let theme_menu = Submenu::with_items(
                "Theme",
                true,
                &themes
                    .iter()
                    .map(|(_, i)| i as &dyn muda::IsMenuItem)
                    .collect::<Vec<_>>(),
            )
            .ok()?;

            let show_disabled = check(ids::SHOW_DISABLED, "Show disabled servers", None);
            let reveal_env = check(ids::REVEAL_ENV, "Reveal environment values", None);
            let sweep = check(ids::SWEEP, "Deep sweep on next scan", None);
            let measure = check(ids::MEASURE, "Measure file sizes", None);
            let rescan = item(ids::RESCAN, "Rescan now", Some("CmdOrCtrl+R"));
            let view_menu = Submenu::with_items(
                "View",
                true,
                &[
                    &tabs[0].1 as &dyn muda::IsMenuItem,
                    &tabs[1].1,
                    &tabs[2].1,
                    &PredefinedMenuItem::separator(),
                    &theme_menu,
                    &PredefinedMenuItem::separator(),
                    &show_disabled,
                    &reveal_env,
                    &PredefinedMenuItem::separator(),
                    &sweep,
                    &measure,
                    &PredefinedMenuItem::separator(),
                    &rescan,
                ],
            )
            .ok()?;

            let window_menu = Submenu::with_items(
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(None) as &dyn muda::IsMenuItem,
                    &PredefinedMenuItem::zoom(None),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::close_window(None),
                ],
            )
            .ok()?;

            let backups = item(ids::BACKUPS, "Open backups folder", None);
            let registry = item(ids::REGISTRY, "Show the client registry location", None);
            let project = item(ids::PROJECT, "Project page", None);
            let help_menu = Submenu::with_items(
                "Help",
                true,
                &[
                    &backups as &dyn muda::IsMenuItem,
                    &registry,
                    &PredefinedMenuItem::separator(),
                    &project,
                ],
            )
            .ok()?;

            let menu = Menu::new();
            menu.append_items(&[&app_menu, &edit_menu, &view_menu, &window_menu, &help_menu])
                .ok()?;
            // Tells macOS which menus own the window list and the help search
            // field, so they behave like every other app's.
            window_menu.set_as_windows_menu_for_nsapp();
            help_menu.set_as_help_menu_for_nsapp();
            menu.init_for_nsapp();

            // A click on the system menu bar is not a window event, so egui has
            // no reason to wake up and would act on it at the next repaint —
            // which, on an idle window, may be never. The handler asks for one.
            let (tx, events) = channel();
            let tx = Mutex::new(tx);
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(event);
                }
                ctx.request_repaint();
            }));

            Some(Self {
                _menu: menu,
                undo,
                select_all,
                select_none,
                find,
                clear_search,
                rescan,
                tabs,
                themes,
                show_disabled,
                reveal_env,
                sweep,
                measure,
                last: Cell::new(None),
                events,
            })
        }

        /// Everything clicked since the last call.
        pub fn drain(&self) -> Vec<MenuAction> {
            self.events
                .try_iter()
                .filter_map(|e| action_for(&e.id.0))
                .collect()
        }

        /// Make the menu agree with the app. Cheap to call every frame.
        pub fn sync(&self, state: &MenuState) {
            if self.last.get().as_ref() == Some(state) {
                return;
            }
            self.last.set(Some(*state));

            self.undo.set_enabled(state.can_undo && state.can_act);
            self.select_all.set_enabled(state.on_servers);
            self.select_none.set_enabled(state.has_checked);
            self.find.set_enabled(state.on_servers);
            self.clear_search.set_enabled(state.has_search);
            self.rescan.set_enabled(state.can_act);

            for (tab, item) in &self.tabs {
                item.set_checked(*tab == state.tab);
            }
            for (mode, item) in &self.themes {
                item.set_checked(*mode == state.theme);
            }
            self.show_disabled.set_checked(state.show_disabled);
            self.reveal_env.set_checked(state.reveal_env);
            self.sweep.set_checked(state.sweep);
            self.measure.set_checked(state.measure);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The ids are the wire between a click and what it does. An id with no
        /// action would be an item that lights up and does nothing.
        #[test]
        fn every_id_maps_to_an_action() {
            for id in [
                ids::UNDO,
                ids::SELECT_ALL,
                ids::SELECT_NONE,
                ids::FIND,
                ids::CLEAR_SEARCH,
                ids::TAB_SERVERS,
                ids::TAB_ORPHANS,
                ids::TAB_COVERAGE,
                ids::THEME_SYSTEM,
                ids::THEME_LIGHT,
                ids::THEME_DARK,
                ids::SHOW_DISABLED,
                ids::REVEAL_ENV,
                ids::SWEEP,
                ids::MEASURE,
                ids::RESCAN,
                ids::BACKUPS,
                ids::REGISTRY,
                ids::PROJECT,
                ids::ABOUT,
            ] {
                assert!(action_for(id).is_some(), "{id} has no action");
            }
        }

        #[test]
        fn an_unknown_id_is_ignored() {
            assert!(action_for("nonsense").is_none());
        }

        /// Every accelerator in the menu is a string literal, and a typo in one
        /// would silently cost the shortcut. They are parsed here instead.
        #[test]
        fn every_accelerator_parses() {
            for s in [
                "CmdOrCtrl+F",
                "CmdOrCtrl+1",
                "CmdOrCtrl+2",
                "CmdOrCtrl+3",
                "CmdOrCtrl+R",
            ] {
                assert!(accel(s).is_some(), "{s} did not parse as an accelerator");
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MenuState;
    use crate::app::MenuAction;
    use eframe::egui;

    /// On every other platform the menu bar is drawn inside the window, so
    /// there is nothing to construct. The type is uninhabited, which is what
    /// makes "there is no native menu here" a fact the compiler knows.
    pub enum NativeMenu {}

    impl NativeMenu {
        pub fn install(_ctx: &egui::Context) -> Option<Self> {
            None
        }

        pub fn drain(&self) -> Vec<MenuAction> {
            match *self {}
        }

        pub fn sync(&self, _state: &MenuState) {
            match *self {}
        }
    }
}
