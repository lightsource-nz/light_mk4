//! The dictaphone with its PORTRAIT interface: the engine is `light_dictaphone_core`,
//! re-exported whole; this crate owns what the interface looks like -- a vertically
//! stacked main page (status line, one big record/stop button, play-last, the recordings
//! entry) and a scrolling recordings list. Held upright; the sibling
//! `light_app_dictaphone_wide` lays the same engine out for glass held sideways.

#![no_std]

pub use light_dictaphone_core::*;

/// The portrait page tree, as statics in the BOARD crate: `PAGE_MAIN` and `PAGE_FILES`,
/// with the board's row gap and touch-target heights baked in -- the metrics are hardware
/// (finger size against pixel density), the tree is not. Pair with
/// [`DisplayConfig::initial_rotation`]` = Rotation::R0` and a portrait rotation map. The
/// status and the flashing recording light both ride the title bar, so the body is just
/// the three action buttons.
///
/// ```ignore
/// light_app_dictaphone::dictaphone_pages! {
///         event: AppEvent, row_gap: 6, list_min_row: 56, rec_min_h: 88
/// }
/// ```
#[macro_export]
macro_rules! dictaphone_pages {
        (event: $event:ty, row_gap: $row_gap:expr, list_min_row: $list_min_row:expr, rec_min_h: $rec_min_h:expr) => {
                static BTN_REC: $crate::Desc<$event> = $crate::Desc::button("* Record").emit(<$event>::Ui($crate::UiAction::RecToggle)).tag($crate::TAG_REC).min_size(0, $rec_min_h);
                static BTN_PLAY: $crate::Desc<$event> = $crate::Desc::button("Play last").emit(<$event>::Ui($crate::UiAction::PlayToggle)).tag($crate::TAG_PLAY).min_size(0, $list_min_row);
                static BTN_FILES: $crate::Desc<$event> = $crate::Desc::button("Recordings >").emit(<$event>::Ui($crate::UiAction::FilesOpen)).navigate(&PAGE_FILES).min_size(0, $list_min_row);
                static MAIN_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Dictaphone").subtitle().linear($row_gap).children(&[&BTN_REC, &BTN_PLAY, &BTN_FILES]);

                static BTN_FILES_BACK: $crate::Desc<$event> = $crate::Desc::button("< Back").back().min_size(0, $list_min_row);
                //   the recordings list is light_ui's reusable picker: eight full-width rows
                // that scroll, each emitting its own index; back rides along as the last row
                $crate::file_list! {
                        FILES_ROWS,
                        event: $event,
                        tag_base: $crate::TAG_ROW_BASE,
                        min_size: (0, $list_min_row),
                        select: |i| <$event>::Ui($crate::UiAction::PlayRow(i)),
                        indices: [0, 1, 2, 3, 4, 5, 6, 7],
                        back: &BTN_FILES_BACK,
                }
                static FILES_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Recordings")
                        .subtitle()
                        .linear($row_gap)
                        .scroll($crate::scroll::VERTICAL)
                        .children(FILES_ROWS);

                static PAGE_MAIN: $crate::Page<$event> = $crate::Page::new(&MAIN_WINDOW, None);
                static PAGE_FILES: $crate::Page<$event> = $crate::Page::new(&FILES_WINDOW, Some(&PAGE_MAIN));
        };
}
