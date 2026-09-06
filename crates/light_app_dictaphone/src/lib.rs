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
                static MAIN_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Dictaphone").stack($row_gap).children(&[&BTN_REC, &BTN_PLAY, &BTN_FILES]);

                static ROW_0: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(0))).tag($crate::TAG_ROW_BASE).min_size(0, $list_min_row);
                static ROW_1: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(1))).tag($crate::TAG_ROW_BASE + 1).min_size(0, $list_min_row);
                static ROW_2: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(2))).tag($crate::TAG_ROW_BASE + 2).min_size(0, $list_min_row);
                static ROW_3: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(3))).tag($crate::TAG_ROW_BASE + 3).min_size(0, $list_min_row);
                static ROW_4: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(4))).tag($crate::TAG_ROW_BASE + 4).min_size(0, $list_min_row);
                static ROW_5: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(5))).tag($crate::TAG_ROW_BASE + 5).min_size(0, $list_min_row);
                static ROW_6: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(6))).tag($crate::TAG_ROW_BASE + 6).min_size(0, $list_min_row);
                static ROW_7: $crate::Desc<$event> = $crate::Desc::button("-").emit(<$event>::Ui($crate::UiAction::PlayRow(7))).tag($crate::TAG_ROW_BASE + 7).min_size(0, $list_min_row);
                static BTN_FILES_BACK: $crate::Desc<$event> = $crate::Desc::button("< Back").back().min_size(0, $list_min_row);
                static FILES_WINDOW: $crate::Desc<$event> = $crate::Desc::window("Recordings")
                        .stack($row_gap)
                        .scroll($crate::scroll::VERTICAL)
                        .children(&[&ROW_0, &ROW_1, &ROW_2, &ROW_3, &ROW_4, &ROW_5, &ROW_6, &ROW_7, &BTN_FILES_BACK]);

                static PAGE_MAIN: $crate::Page<$event> = $crate::Page::new(&MAIN_WINDOW, None);
                static PAGE_FILES: $crate::Page<$event> = $crate::Page::new(&FILES_WINDOW, Some(&PAGE_MAIN));
        };
}
