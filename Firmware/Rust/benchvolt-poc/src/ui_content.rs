pub const MAIN_MENU_ITEMS: [&str; 5] = ["DC Power", "AWG", "Settings", "System", "Help"];
pub const SETTINGS_ITEM_COUNT: u8 = 5;
pub const PROFILE_ITEM_COUNT: u8 = 4;
pub const AWG_ITEM_COUNT: u8 = 8;

pub const HELP_VISIBLE_LINES: u8 = 7;
pub const HELP_SCROLL_STEP: u8 = 5;
pub const HELP_TEXT: &str = "MAIN MENU\nPower - five DC outputs\nAWG - CH4 / CH5 waveforms\nSettings - save / restore\nSystem - version and status\nNAVIGATION\nLong press - go back\nClick - move focus\nTurn - select or edit\nPOWER\nOverview shows output status.\nClick to focus, turn to toggle.\nWith no focus, turn screens.\nClick through Output, Voltage,\nCV/CC, and Current Limit.\nTurn to edit, click to finish.\nCV / CC\nCH4 and CH5 support CC.\nSet CC and Current Limit.\nAWG\nCH4 or CH5, up to 120 Hz.\nSquare includes duty cycle.\nSet Low, High, then Start.\nLoad: RMS A and average W.";

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
        assert_eq!(HELP_MAX_SCROLL, 17);
        assert_eq!(HELP_MAX_SCROLL + HELP_VISIBLE_LINES, HELP_LINE_COUNT);
    }
}
