//! Large letters for the claim key, so it can be read from across a room.
//!
//! The Linux console's font has no large sizes, so each character is drawn
//! from a 3 by 5 grid of full blocks (`█`, which the console's font has:
//! it's in the PC's original character set). Only what a claim key holds
//! is here: hex digits and the dash between groups.

/// How many rows a large character takes.
pub const HEIGHT: usize = 5;

/// One character's grid, top row first, or `None` for one it can't draw.
fn glyph(character: char) -> Option<[&'static str; HEIGHT]> {
    Some(match character.to_ascii_lowercase() {
        '0' => ["###", "# #", "# #", "# #", "###"],
        '1' => [" # ", "## ", " # ", " # ", "###"],
        '2' => ["###", "  #", "###", "#  ", "###"],
        '3' => ["###", "  #", " ##", "  #", "###"],
        '4' => ["# #", "# #", "###", "  #", "  #"],
        '5' => ["###", "#  ", "###", "  #", "###"],
        '6' => ["###", "#  ", "###", "# #", "###"],
        '7' => ["###", "  #", "  #", "  #", "  #"],
        '8' => ["###", "# #", "###", "# #", "###"],
        '9' => ["###", "# #", "###", "  #", "###"],
        'a' => [" # ", "# #", "###", "# #", "# #"],
        'b' => ["## ", "# #", "## ", "# #", "## "],
        'c' => ["###", "#  ", "#  ", "#  ", "###"],
        'd' => ["## ", "# #", "# #", "# #", "## "],
        'e' => ["###", "#  ", "## ", "#  ", "###"],
        'f' => ["###", "#  ", "## ", "#  ", "#  "],
        '-' => ["   ", "   ", "###", "   ", "   "],
        _ => return None,
    })
}

/// How many columns `text` takes in large letters.
pub fn width(text: &str) -> usize {
    let count = text.chars().count();
    (count * 4).saturating_sub(1)
}

/// `text` in large letters, [`HEIGHT`] rows, or `None` if it holds a
/// character there's no large letter for.
pub fn render(text: &str) -> Option<Vec<String>> {
    let glyphs = text.chars().map(glyph).collect::<Option<Vec<_>>>()?;
    let rows = (0..HEIGHT)
        .map(|row| {
            glyphs
                .iter()
                .map(|glyph| glyph[row].replace('#', "█"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_key_fits_an_80_column_screen() {
        let key = "3f9a-12bc-77de-0a41";
        let rows = render(key).unwrap();
        assert_eq!(rows.len(), HEIGHT);
        assert_eq!(width(key), 75);
        for row in &rows {
            assert_eq!(row.chars().count(), width(key));
        }
    }

    #[test]
    fn a_character_with_no_large_letter_draws_nothing() {
        assert_eq!(render("3f9a?"), None);
    }

    #[test]
    fn letters_are_drawn_in_full_blocks() {
        assert_eq!(
            render("1-").unwrap(),
            [" █     ", "██     ", " █  ███", " █     ", "███    "]
        );
    }
}
