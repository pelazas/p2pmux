use crate::term::BufWrite as _;

/// Represents a foreground or background color for cells.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum Color {
    /// The default terminal color.
    #[default]
    Default,

    /// An indexed terminal color.
    Idx(u8),

    /// An RGB terminal color. The parameters are (red, green, blue).
    Rgb(u8, u8, u8),
}

const TEXT_MODE_INTENSITY: u16 = 0b0000_0011;
const TEXT_MODE_BOLD: u16 = 0b0000_0001;
const TEXT_MODE_DIM: u16 = 0b0000_0010;
const TEXT_MODE_ITALIC: u16 = 0b0000_0100;
const TEXT_MODE_INVERSE: u16 = 0b0001_0000;
const TEXT_MODE_STRIKE: u16 = 0b0010_0000;
const TEXT_MODE_BLINK: u16 = 0b0100_0000;
const TEXT_MODE_CONCEAL: u16 = 0b1000_0000;
const TEXT_MODE_OVERLINE: u16 = 0b0001_0000_0000;

/// How an underline is drawn. `Single` is SGR 4; the others are the styled
/// forms programs actually ask for and that a plain `4` cannot stand in for.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Underline {
    /// No underline.
    #[default]
    None,
    /// SGR 4.
    Single,
    /// SGR 21, and also `4:2`.
    Double,
    /// SGR `4:3`.
    Curly,
    /// SGR `4:4`.
    Dotted,
    /// SGR `4:5`.
    Dashed,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    pub underline_color: Color,
    pub underline: Underline,
    pub mode: u16,
}

impl Attrs {
    pub fn bold(&self) -> bool {
        self.mode & TEXT_MODE_BOLD != 0
    }

    pub fn dim(&self) -> bool {
        self.mode & TEXT_MODE_DIM != 0
    }

    fn intensity(&self) -> u16 {
        self.mode & TEXT_MODE_INTENSITY
    }

    pub fn set_bold(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_BOLD;
    }

    pub fn set_dim(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_DIM;
    }

    pub fn set_normal_intensity(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
    }

    pub fn italic(&self) -> bool {
        self.mode & TEXT_MODE_ITALIC != 0
    }

    pub fn set_italic(&mut self, italic: bool) {
        if italic {
            self.mode |= TEXT_MODE_ITALIC;
        } else {
            self.mode &= !TEXT_MODE_ITALIC;
        }
    }

    pub fn underline(&self) -> bool {
        self.underline != Underline::None
    }

    pub fn underline_style(&self) -> Underline {
        self.underline
    }

    pub fn set_underline(&mut self, underline: bool) {
        self.underline = if underline {
            Underline::Single
        } else {
            Underline::None
        };
    }

    pub fn set_underline_style(&mut self, underline: Underline) {
        self.underline = underline;
    }

    pub fn strikethrough(&self) -> bool {
        self.mode & TEXT_MODE_STRIKE != 0
    }

    pub fn set_strikethrough(&mut self, strikethrough: bool) {
        self.set_flag(TEXT_MODE_STRIKE, strikethrough);
    }

    pub fn blink(&self) -> bool {
        self.mode & TEXT_MODE_BLINK != 0
    }

    pub fn set_blink(&mut self, blink: bool) {
        self.set_flag(TEXT_MODE_BLINK, blink);
    }

    pub fn conceal(&self) -> bool {
        self.mode & TEXT_MODE_CONCEAL != 0
    }

    pub fn set_conceal(&mut self, conceal: bool) {
        self.set_flag(TEXT_MODE_CONCEAL, conceal);
    }

    pub fn overline(&self) -> bool {
        self.mode & TEXT_MODE_OVERLINE != 0
    }

    pub fn set_overline(&mut self, overline: bool) {
        self.set_flag(TEXT_MODE_OVERLINE, overline);
    }

    fn set_flag(&mut self, flag: u16, enabled: bool) {
        if enabled {
            self.mode |= flag;
        } else {
            self.mode &= !flag;
        }
    }

    pub fn inverse(&self) -> bool {
        self.mode & TEXT_MODE_INVERSE != 0
    }

    pub fn set_inverse(&mut self, inverse: bool) {
        if inverse {
            self.mode |= TEXT_MODE_INVERSE;
        } else {
            self.mode &= !TEXT_MODE_INVERSE;
        }
    }

    pub fn write_escape_code_diff(&self, contents: &mut Vec<u8>, other: &Self) {
        if self != other && self == &Self::default() {
            crate::term::ClearAttrs.write_buf(contents);
            return;
        }

        let attrs = crate::term::Attrs::default();

        let attrs = if self.fgcolor == other.fgcolor {
            attrs
        } else {
            attrs.fgcolor(self.fgcolor)
        };
        let attrs = if self.bgcolor == other.bgcolor {
            attrs
        } else {
            attrs.bgcolor(self.bgcolor)
        };
        let attrs = if self.intensity() == other.intensity() {
            attrs
        } else {
            attrs.intensity(match self.intensity() {
                0 => crate::term::Intensity::Normal,
                TEXT_MODE_BOLD => crate::term::Intensity::Bold,
                TEXT_MODE_DIM => crate::term::Intensity::Dim,
                _ => unreachable!(),
            })
        };
        let attrs = if self.italic() == other.italic() {
            attrs
        } else {
            attrs.italic(self.italic())
        };
        let attrs = if self.inverse() == other.inverse() {
            attrs
        } else {
            attrs.inverse(self.inverse())
        };

        attrs.write_buf(contents);
        self.write_extended_diff(contents, other);
    }

    /// Attributes the stock escape writer does not know: styled underlines,
    /// underline colour, strike, blink, conceal and overline.
    fn write_extended_diff(&self, contents: &mut Vec<u8>, other: &Self) {
        if self.underline != other.underline {
            let sequence: &[u8] = match self.underline {
                Underline::None => b"\x1b[24m",
                Underline::Single => b"\x1b[4m",
                Underline::Double => b"\x1b[21m",
                Underline::Curly => b"\x1b[4:3m",
                Underline::Dotted => b"\x1b[4:4m",
                Underline::Dashed => b"\x1b[4:5m",
            };
            contents.extend_from_slice(sequence);
        }
        if self.underline_color != other.underline_color {
            match self.underline_color {
                Color::Default => contents.extend_from_slice(b"\x1b[59m"),
                Color::Idx(index) => {
                    contents.extend_from_slice(b"\x1b[58;5;");
                    extend_index(contents, index);
                    contents.push(b'm');
                }
                Color::Rgb(red, green, blue) => {
                    contents.extend_from_slice(b"\x1b[58;2;");
                    extend_index(contents, red);
                    contents.push(b';');
                    extend_index(contents, green);
                    contents.push(b';');
                    extend_index(contents, blue);
                    contents.push(b'm');
                }
            }
        }
        write_flag(
            contents,
            self.strikethrough(),
            other.strikethrough(),
            b"\x1b[9m",
            b"\x1b[29m",
        );
        write_flag(
            contents,
            self.blink(),
            other.blink(),
            b"\x1b[5m",
            b"\x1b[25m",
        );
        write_flag(
            contents,
            self.conceal(),
            other.conceal(),
            b"\x1b[8m",
            b"\x1b[28m",
        );
        write_flag(
            contents,
            self.overline(),
            other.overline(),
            b"\x1b[53m",
            b"\x1b[55m",
        );
    }
}

fn write_flag(contents: &mut Vec<u8>, enabled: bool, was: bool, on: &[u8], off: &[u8]) {
    if enabled != was {
        contents.extend_from_slice(if enabled { on } else { off });
    }
}

fn extend_index(contents: &mut Vec<u8>, value: u8) {
    contents.extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
}
