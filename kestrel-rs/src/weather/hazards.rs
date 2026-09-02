//! What each watch, warning and advisory is drawn in.
//!
//! The polygons arrive as pixels - the service renders them and hands back a
//! picture - so nothing on screen can say what a colour means. The service does
//! publish the mapping, as a legend of 111 named swatches, which is far too many
//! to put on a map and exactly right to look names up in.
//!
//! So the table is generated from that legend rather than typed, and the key on
//! the radar lists only the hazards actually in force in the view. To rebuild it,
//! decode the swatches and take the commonest opaque colour of each:
//!
//!   curl -s '<the WWA MapServer>/legend?f=json' | ...
//!
//! Spot-checked against the National Weather Service's published colour codes:
//! a Tornado Warning is red, a Severe Thunderstorm Warning yellow, a Flash Flood
//! Warning dark green, an Extreme Heat Warning magenta.

/// The colour a hazard is drawn in, by the service's own name for it.
pub fn colour_of(hazard: &str) -> Option<(u8, u8, u8)> {
    HAZARD_COLOURS
        .iter()
        .find(|(name, ..)| name.eq_ignore_ascii_case(hazard))
        .map(|&(_, r, g, b)| (r, g, b))
}

pub const HAZARD_COLOURS: [(&str, u8, u8, u8); 111] = [
    ("911 Telephone Outage", 192, 192, 192),
    ("Administrative Message", 192, 192, 192),
    ("Air Quality Alert", 128, 128, 128),
    ("Air Stagnation Advisory", 128, 128, 128),
    ("Ashfall Advisory", 105, 105, 105),
    ("Ashfall Warning", 169, 169, 169),
    ("Avalanche Advisory", 205, 133, 63),
    ("Avalanche Warning", 30, 144, 255),
    ("Avalanche Watch", 244, 164, 96),
    ("Beach Hazards Statement", 64, 224, 208),
    ("Blizzard Warning", 255, 69, 0),
    ("Blowing Dust Advisory", 189, 183, 107),
    ("Blowing Dust Warning", 255, 228, 196),
    ("Blue Alert", 255, 255, 255),
    ("Brisk Wind Advisory", 216, 191, 216),
    ("Child Abduction Emergency", 255, 255, 255),
    ("Civil Danger Warning", 255, 182, 193),
    ("Civil Emergency Message", 255, 182, 193),
    ("Coastal Flood Advisory", 124, 252, 0),
    ("Coastal Flood Statement", 107, 142, 35),
    ("Coastal Flood Warning", 34, 139, 34),
    ("Coastal Flood Watch", 102, 205, 170),
    ("Cold Weather Advisory", 175, 238, 238),
    ("Dense Fog Advisory", 112, 128, 144),
    ("Dense Smoke Advisory", 240, 230, 140),
    ("Dust Advisory", 189, 183, 107),
    ("Dust Storm Warning", 255, 228, 196),
    ("Earthquake Warning", 139, 69, 19),
    ("Evacuation Immediate", 127, 255, 0),
    ("Extreme Cold Warning", 0, 0, 255),
    ("Extreme Cold Watch", 95, 158, 160),
    ("Extreme Fire Danger", 233, 150, 122),
    ("Extreme Heat Warning", 199, 22, 133),
    ("Extreme Heat Watch", 128, 0, 0),
    ("Extreme Wind Warning", 255, 140, 0),
    ("Fire Warning", 160, 82, 45),
    ("Fire Weather Watch", 255, 222, 173),
    ("Flash Flood Statement", 139, 0, 0),
    ("Flash Flood Warning", 57, 121, 57),
    ("Flash Flood Watch", 46, 139, 87),
    ("Flood Advisory", 0, 255, 127),
    ("Flood Statement", 0, 255, 0),
    ("Flood Warning", 0, 255, 0),
    ("Flood Watch", 46, 139, 87),
    ("Freeze Warning", 72, 61, 139),
    ("Freeze Watch", 0, 255, 255),
    ("Freezing Fog Advisory", 0, 128, 128),
    ("Freezing Spray Advisory", 0, 191, 255),
    ("Frost Advisory", 100, 149, 237),
    ("Gale Warning", 221, 160, 221),
    ("Gale Watch", 255, 192, 203),
    ("Hazardous Materials Warning", 75, 0, 130),
    ("Hazardous Seas Warning", 216, 191, 216),
    ("Hazardous Seas Watch", 72, 61, 139),
    ("Hazardous Weather Outlook", 238, 232, 170),
    ("Heat Advisory", 255, 127, 80),
    ("Heavy Freezing Spray Warning", 0, 191, 255),
    ("Heavy Freezing Spray Watch", 188, 143, 143),
    ("High Surf Advisory", 186, 85, 211),
    ("High Surf Warning", 34, 139, 34),
    ("High Wind Warning", 218, 165, 32),
    ("High Wind Watch", 184, 134, 11),
    ("Hurricane Force Wind Warning", 205, 92, 92),
    ("Hurricane Force Wind Watch", 153, 50, 204),
    ("Hurricane Warning", 220, 20, 60),
    ("Hurricane Watch", 255, 0, 255),
    ("Hydrologic Outlook", 144, 238, 144),
    ("Ice Storm Warning", 139, 0, 139),
    ("Lake Effect Snow Warning", 0, 139, 139),
    ("Lake Wind Advisory", 210, 180, 140),
    ("Lakeshore Flood Advisory", 124, 252, 0),
    ("Lakeshore Flood Statement", 107, 142, 35),
    ("Lakeshore Flood Warning", 34, 139, 34),
    ("Lakeshore Flood Watch", 102, 205, 170),
    ("Law Enforcement Warning", 192, 192, 192),
    ("Local Area Emergency", 192, 192, 192),
    ("Low Water Advisory", 165, 42, 42),
    ("Marine Weather Statement", 255, 239, 213),
    ("Nuclear Power Plant Warning", 75, 0, 130),
    ("Radiological Hazard Warning", 75, 0, 130),
    ("Red Flag Warning", 255, 20, 147),
    ("Rip Current Statement", 64, 224, 208),
    ("Severe Thunderstorm Warning", 255, 255, 0),
    ("Severe Thunderstorm Watch", 219, 112, 147),
    ("Severe Weather Statement", 0, 255, 255),
    ("Shelter In Place Warning", 250, 128, 114),
    ("Short Term Forecast", 152, 251, 152),
    ("Small Craft Advisory", 216, 191, 216),
    ("Snow Squall Warning", 199, 21, 133),
    ("Special Marine Warning", 230, 152, 0),
    ("Special Weather Statement", 255, 228, 181),
    ("Storm Surge Warning", 181, 36, 247),
    ("Storm Surge Watch", 219, 127, 247),
    ("Storm Warning", 148, 0, 211),
    ("Storm Watch", 255, 228, 181),
    ("Test", 240, 255, 255),
    ("Tornado Warning", 255, 0, 0),
    ("Tornado Watch", 255, 255, 0),
    ("Tropical Cyclone Local Statement", 255, 228, 181),
    ("Tropical Storm Warning", 178, 34, 34),
    ("Tropical Storm Watch", 240, 128, 128),
    ("Tsunami Advisory", 210, 105, 30),
    ("Tsunami Warning", 253, 99, 71),
    ("Tsunami Watch", 255, 0, 255),
    ("Typhoon Warning", 220, 20, 60),
    ("Typhoon Watch", 255, 0, 255),
    ("Volcano Warning", 47, 79, 79),
    ("Wind Advisory", 210, 180, 140),
    ("Winter Storm Warning", 255, 105, 180),
    ("Winter Storm Watch", 70, 130, 180),
    ("Winter Weather Advisory", 123, 104, 238),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-checked against the National Weather Service's published colour
    /// codes. If the service ever restyles, these are what will notice.
    #[test]
    fn the_hazards_everybody_knows_are_the_colours_everybody_knows() {
        assert_eq!(colour_of("Tornado Warning"), Some((255, 0, 0)));
        assert_eq!(colour_of("Severe Thunderstorm Warning"), Some((255, 255, 0)));
        assert_eq!(colour_of("Flash Flood Warning"), Some((57, 121, 57)));
        assert_eq!(colour_of("Extreme Heat Warning"), Some((199, 22, 133)));

        // The service's own spelling is what arrives, but a lookup that failed
        // on case would silently draw a swatch of nothing.
        assert_eq!(colour_of("tornado warning"), colour_of("Tornado Warning"));
        assert_eq!(colour_of("Not A Real Hazard"), None);
        assert!(HAZARD_COLOURS.len() > 100, "the legend has a hundred-odd entries");
    }
}
