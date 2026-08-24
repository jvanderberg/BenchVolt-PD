pub const MAIN_MENU_ITEMS: [&str; 5] = ["DC Power", "AWG", "Settings", "System", "Help"];
pub const SETTINGS_ITEM_COUNT: u8 = 5;
pub const PROFILE_ITEM_COUNT: u8 = 4;
pub const AWG_ITEM_COUNT: u8 = 8;

pub const HELP_VISIBLE_LINES: u8 = 7;
pub const HELP_SCROLL_STEP: u8 = 5;
pub const HELP_TEXT: &str = "MAIN MENU\nSelect an item:\nPower - control all five DC outputs\nAWG - waveforms on CH4 / CH5\nSettings - save / restore settings\nSystem - firmware version and status\nHelp - this guide\nNAVIGATION\nLong press - go back\nClick - move focus between controls\nTurn - menu selection or control edit\nPOWER SCREENS\nOverview shows all outputs and status.\nClick to focus a channel switch.\nTurn either way to toggle it.\nClick past CH5 to finish.\nWith no focus, turn between screens.\nOn a channel, click through Output,\nVoltage, CV/CC, and Current Limit.\nTurn to edit the focused control.\nClick until focus clears to navigate.\nCV / CC\nCH4 and CH5 support CC mode.\nSelect CC, then set Current Limit.\nThe loop lowers voltage to hold current.\nCC turns green while regulating.\nAWG\nGenerates waveforms on CH4 or CH5.\nTurn, click to edit, turn, then click.\nSine, triangle, ramp, and square.\nFrequency is available up to 120 Hz.\nSquare adds a duty-cycle setting.\nSet Low and High, then choose Start.\nRMS amps and average watts appear\nin the load panel on the right.";

const fn line_count(text: &str) -> u8 {
    let bytes = text.as_bytes();
    let mut lines = 1u8;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            lines += 1;
        }
        index += 1;
    }
    lines
}

pub const HELP_LINE_COUNT: u8 = line_count(HELP_TEXT);
pub const HELP_MAX_SCROLL: u8 = HELP_LINE_COUNT - HELP_VISIBLE_LINES;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_counts_and_help_bounds_come_from_the_painted_content() {
        assert_eq!(MAIN_MENU_ITEMS.len(), 5);
        assert_eq!(HELP_TEXT.lines().count(), usize::from(HELP_LINE_COUNT));
        assert_eq!(HELP_MAX_SCROLL, 28);
        assert_eq!(HELP_MAX_SCROLL + HELP_VISIBLE_LINES, HELP_LINE_COUNT);
    }
}
