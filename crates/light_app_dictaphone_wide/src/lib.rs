//! The dictaphone with a LANDSCAPE interface: the engine is `light_dictaphone_core`,
//! re-exported whole and identical to the portrait app's -- this crate owns only what the
//! interface looks like when the glass is held sideways. The main page runs LEFT TO
//! RIGHT: the status column, the record/stop button at exactly its text's width,
//! play-last, and the recordings entry, side by side at full height (the toolkit's `Row`
//! layout). The recordings list runs left to right too -- full-height columns wide enough
//! to print a short file name, dragged sideways through the frame.
//!
//! Pair with [`DisplayConfig::initial_rotation`]` = Rotation::R90` and a landscape
//! rotation map: the interface starts sideways and follows the device between the two
//! landscape poses.

#![no_std]

pub use light_dictaphone_core::*;

/// The landscape page tree, as statics in the BOARD crate: `PAGE_MAIN` and `PAGE_FILES`,
/// with the board's gap and text metrics baked in. `rec_w` FIXES the record button's
/// width -- just wide enough for its text, sized against the board's font; the freed
/// width goes to the play and recordings columns, so their labels stay legible.
/// The status and the flashing recording light both ride the title bar. `list_col_w` is a
/// recordings column: wide enough to print a short file name in full, and the reason the
/// list scrolls -- eight such columns outrun any bar, and the surplus drags in from the
/// right. The back button (`back_w`, fixed like `rec_w`) is PINNED outside the scrolling
/// strip, always at the left edge: the strip consumes horizontal drags as scrolling, so a
/// swipe cannot leave the page and the way back must stay under a thumb.
///
/// ```ignore
/// light_app_dictaphone_wide::dictaphone_wide_pages! {
///         event: AppEvent, gap: 6, rec_w: 110, list_col_w: 120, back_w: 84
/// }
/// ```
#[macro_export]
macro_rules! dictaphone_wide_pages {
        (event: $event:ty, gap: $gap:expr, rec_w: $rec_w:expr, list_col_w: $list_col_w:expr, back_w: $back_w:expr) => {
                static BTN_REC: $crate::Desc<$event> = $crate::Desc::button("* Record").emit(<$event>::Ui($crate::UiAction::RecToggle)).tag($crate::TAG_REC).min_size($rec_w, 0).max_size($rec_w, 0);
                static BTN_PLAY: $crate::Desc<$event> = $crate::Desc::button("Play last").emit(<$event>::Ui($crate::UiAction::PlayToggle)).tag($crate::TAG_PLAY);
                static BTN_FILES: $crate::Desc<$event> = $crate::Desc::button("Recordings >").emit(<$event>::Ui($crate::UiAction::FilesOpen)).navigate(&PAGE_FILES);
                static MAIN_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Dictaphone").linear($gap).children(&[&BTN_REC, &BTN_PLAY, &BTN_FILES]);

                //   the recordings list is light_ui's reusable picker: eight full-height
                // columns that drag sideways, each emitting its own index. Back is NOT part of
                // the list here -- it is pinned outside the scrolling strip (below)
                $crate::file_list! {
                        FILES_ROWS,
                        event: $event,
                        tag_base: $crate::TAG_ROW_BASE,
                        min_size: ($list_col_w, 0),
                        select: |i| <$event>::Ui($crate::UiAction::PlayRow(i)),
                        indices: [0, 1, 2, 3, 4, 5, 6, 7],
                }
                static BTN_FILES_BACK: $crate::Desc<$event> = $crate::Desc::button("< Back").back().min_size($back_w, 0).max_size($back_w, 0);
                //   the paging buttons are pinned outside the strip like back is: the strip eats
                // horizontal drags, so a "next page" the reader had to scroll to reach would be
                // unreachable. They show only when that page exists; hidden, they collapse out
                static BTN_FILES_PREV: $crate::Desc<$event> = $crate::Desc::button("< Newer").emit(<$event>::Ui($crate::UiAction::FilesPrev)).tag($crate::TAG_PREV).min_size($back_w, 0).max_size($back_w, 0);
                static BTN_FILES_NEXT: $crate::Desc<$event> = $crate::Desc::button("Older >").emit(<$event>::Ui($crate::UiAction::FilesNext)).tag($crate::TAG_NEXT).min_size($back_w, 0).max_size($back_w, 0);
                static FILES_STRIP: $crate::Desc<$event> = $crate::Desc::frame()
                        .grow()
                        .linear($gap)
                        .scroll($crate::scroll::HORIZONTAL)
                        .children(FILES_ROWS);
                static FILES_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Recordings")
                        .linear($gap)
                        .children(&[&BTN_FILES_BACK, &BTN_FILES_PREV, &FILES_STRIP, &BTN_FILES_NEXT]);

                static PAGE_MAIN: $crate::Page<$event> = $crate::Page::new(&MAIN_WINDOW, None);
                static PAGE_FILES: $crate::Page<$event> = $crate::Page::new(&FILES_WINDOW, Some(&PAGE_MAIN));
        };
}
