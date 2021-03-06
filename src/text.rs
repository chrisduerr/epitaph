//! OpenGL text rendering.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::error::Error;
use std::result::Result as StdResult;
use std::{cmp, mem, ptr};

use crossfont::{
    BitmapBuffer, FontDesc, FontKey, GlyphKey, Metrics, Rasterize, RasterizedGlyph,
    Rasterizer, Size as FontSize, Slant, Style, Weight,
};
use tiny_skia::{Pixmap, Transform};
use usvg::{FitTo, Options, Tree};

use crate::gl;
use crate::gl::types::GLuint;

/// Width and height of the glyph atlas texture.
const ATLAS_SIZE: i32 = 1024;

/// Convenience result wrapper.
type Result<T> = StdResult<T, Box<dyn Error>>;

/// Cached OpenGL rasterization.
pub struct GlRasterizer {
    // OpenGL subtexture caching.
    cache: HashMap<CacheKey, GlSubTexture>,
    atlas: Atlas,

    // FreeType font rasterization.
    rasterizer: Rasterizer,
    size: FontSize,
    font: FontKey,
}

impl GlRasterizer {
    pub fn new(font: &str, size: impl Into<FontSize>) -> Result<Self> {
        let size = size.into();

        // Create FreeType rasterizer.
        let mut rasterizer = Rasterizer::new(1.)?;

        // Load font at the requested size.
        let font_style = Style::Description { slant: Slant::Normal, weight: Weight::Normal };
        let font_desc = FontDesc::new(font, font_style);
        let font = rasterizer.load_font(&font_desc, size)?;

        Ok(Self {
            rasterizer,
            font,
            size,
            atlas: Default::default(),
            cache: Default::default(),
        })
    }

    /// Rasterize each glyph in a string.
    ///
    /// Returns an iterator over all glyphs. The advance stored on each glyph
    /// has the correct kerning applied already.
    ///
    /// If any of the glyphs cannot be rasterized, all glyphs up to that point
    /// will be returned.
    pub fn rasterize_string<'a>(
        &'a mut self,
        text: &'a str,
    ) -> impl Iterator<Item = GlSubTexture> + 'a {
        text.chars().scan(self.glyph_key(' '), |glyph_key, c| {
            let mut glyph = self.rasterize_char(c).ok()?;

            // Add kerning to glyph advance.
            let last_key = mem::replace(glyph_key, self.glyph_key(c));
            let kerning = self.rasterizer.kerning(last_key, *glyph_key);
            glyph.advance.0 += kerning.0 as i32;
            glyph.advance.1 += kerning.1 as i32;

            Some(glyph)
        })
    }

    /// Get rasterized OpenGL glyph.
    pub fn rasterize_char(&mut self, character: char) -> Result<GlSubTexture> {
        let glyph_key = self.glyph_key(character);

        // Try to load glyph from cache.
        let entry = match self.cache.entry(character.into()) {
            Entry::Occupied(entry) => return Ok(*entry.get()),
            Entry::Vacant(entry) => entry,
        };

        // Rasterize the glyph if it's missing.
        let rasterized_glyph = self.rasterizer.get_glyph(glyph_key)?;
        let glyph = self.atlas.insert(&rasterized_glyph)?;

        Ok(*entry.insert(glyph))
    }

    /// Rasterize an SVG from its text.
    pub fn rasterize_svg(&mut self, svg: Svg) -> Result<GlSubTexture> {
        // Try to lead svg from cache.
        let entry = match self.cache.entry(svg.into()) {
            Entry::Occupied(entry) => return Ok(*entry.get()),
            Entry::Vacant(entry) => entry,
        };

        let (width, height) = svg.size();

        // Setup target buffer.
        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| format!("Invalid SVG buffer size: {width}x{height}"))?;

        // Render SVG into buffer.
        let tree = Tree::from_str(svg.content(), &Options::default().to_ref())?;
        let size = FitTo::Size(width, height);
        resvg::render(&tree, size, Transform::default(), pixmap.as_mut())
            .ok_or_else(|| format!("Invalid SVG target size: {width}x{height}"))?;

        // Load SVG into atlas.
        let atlas_entry = AtlasEntry::new_svg(pixmap.take(), width, height);
        let svg = self.atlas.insert(atlas_entry)?;

        Ok(*entry.insert(svg))
    }

    /// Get font metrics.
    pub fn metrics(&self) -> Result<Metrics> {
        Ok(self.rasterizer.metrics(self.font, self.size)?)
    }

    /// Get glyph key for a character.
    fn glyph_key(&self, character: char) -> GlyphKey {
        GlyphKey { font_key: self.font, size: self.size, character }
    }
}

/// Atlas for combining multiple textures in OpenGL.
///
/// The strategy for filling an atlas looks roughly like this:
///
/// ```text
///                           (width, height)
///   ?????????????????????????????????????????????????????????????????????????????????????????????
///   ??? 10  ???     ???     ???     ???     ??? <- Atlas is full when next glyph's height doesn't fit.
///   ???     ???     ???     ???     ???     ??? <- Empty spaces for new elements.
///   ?????????????????????????????????????????????????????????????????????????????????????????????
///   ??? 5   ??? 6   ??? 7   ??? 8   ??? 9   ???
///   ???     ???     ???     ???     ???     ???
///   ????????????????????????????????????????????????????????????????????????????????????????????? <- Row height is tallest subtexture in the row.
///   ??? 1   ??? 2   ??? 3   ??? 4         ???    This is the baseline for the next row.
///   ???     ???     ???     ???           ??? <- Row is full when next glyph's width doesn't fit.
///   ?????????????????????????????????????????????????????????????????????????????????????????????
/// (0, 0)
/// ```
pub struct Atlas {
    /// OpenGL texture ID.
    textures: Vec<GLuint>,
    /// Largest glyph's height in this row.
    row_height: i32,
    /// X position for writing new glyphs.
    cursor_x: i32,
    /// Y position for writing new glyphs.
    cursor_y: i32,
}

impl Default for Atlas {
    fn default() -> Self {
        Self {
            textures: vec![Self::create_texture()],
            row_height: Default::default(),
            cursor_x: Default::default(),
            cursor_y: Default::default(),
        }
    }
}

impl Drop for Atlas {
    fn drop(&mut self) {
        for texture in &self.textures {
            unsafe {
                gl::DeleteTextures(1, texture);
            }
        }
    }
}

impl Atlas {
    /// Insert an entry into the atlas.
    fn insert<'a, E: Into<AtlasEntry<'a>>>(&mut self, entry: E) -> Result<GlSubTexture> {
        let entry = entry.into();

        // Error if entry cannot fit at all.
        if self.cursor_x > ATLAS_SIZE || self.cursor_y > ATLAS_SIZE {
            return Err("atlas is full".into());
        }

        // Create new row if entry doesn't fit into current one.
        if self.cursor_x + entry.width > ATLAS_SIZE {
            self.cursor_y += mem::take(&mut self.row_height);
            self.cursor_x = 0;
        }

        // Create a new texture if the row's available height is too little.
        if self.cursor_y + entry.height > ATLAS_SIZE {
            self.textures.push(Self::create_texture());
            self.row_height = 0;
            self.cursor_x = 0;
            self.cursor_y = 0;
        }

        // Upload entry's buffer to OpenGL.
        let active_texture = self.textures[self.textures.len() - 1];
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, active_texture);

            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                self.cursor_x,
                self.cursor_y,
                entry.width,
                entry.height,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                entry.buffer.as_ptr() as *const _,
            );

            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        // Generate UV coordinates.
        let uv_bot = self.cursor_y as f32 / ATLAS_SIZE as f32;
        let uv_left = self.cursor_x as f32 / ATLAS_SIZE as f32;
        let uv_height = entry.height as f32 / ATLAS_SIZE as f32;
        let uv_width = entry.width as f32 / ATLAS_SIZE as f32;

        // Update atlas write position.
        self.row_height = cmp::max(self.row_height, entry.height);
        self.cursor_x += entry.width;

        Ok(GlSubTexture {
            uv_height,
            uv_width,
            uv_left,
            uv_bot,
            multicolor: entry.multicolor,
            texture_id: active_texture,
            advance: entry.advance,
            height: entry.height as i16,
            width: entry.width as i16,
            left: entry.left as i16,
            top: entry.top as i16,
        })
    }

    /// Create a new atlas texture.
    fn create_texture() -> GLuint {
        let mut texture_id = 0;
        unsafe {
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::GenTextures(1, &mut texture_id);
            gl::BindTexture(gl::TEXTURE_2D, texture_id);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA as i32,
                ATLAS_SIZE,
                ATLAS_SIZE,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                ptr::null(),
            );
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
        texture_id
    }
}

/// Subtexture cached inside an [`Atlas`].
#[derive(Copy, Clone, Debug)]
pub struct GlSubTexture {
    pub texture_id: GLuint,
    pub multicolor: bool,
    pub top: i16,
    pub left: i16,
    pub width: i16,
    pub height: i16,
    pub uv_bot: f32,
    pub uv_left: f32,
    pub uv_width: f32,
    pub uv_height: f32,
    pub advance: (i32, i32),
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let rgb_len = rgb.len();
    debug_assert_eq!(rgb_len % 3, 0);

    let pixel_count = rgb_len / 3;
    let mut rgba = vec![255; pixel_count * 4];

    for (rgb, rgba) in rgb.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
        rgba[..3].copy_from_slice(rgb);
    }

    rgba
}

/// Element stored in the texture atlas.
struct AtlasEntry<'a> {
    buffer: Cow<'a, Vec<u8>>,
    width: i32,
    height: i32,
    top: i32,
    left: i32,
    advance: (i32, i32),
    multicolor: bool,
}

impl AtlasEntry<'static> {
    /// Create a new SVG atlas entry.
    fn new_svg(buffer: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            buffer: Cow::Owned(buffer),
            width: width as i32,
            height: height as i32,
            top: 0,
            left: 0,
            advance: (width as i32, 0),
            multicolor: true,
        }
    }
}

impl<'a> From<&'a RasterizedGlyph> for AtlasEntry<'a> {
    fn from(glyph: &'a RasterizedGlyph) -> Self {
        let (buffer, multicolor) = match &glyph.buffer {
            BitmapBuffer::Rgb(buffer) => (Cow::Owned(rgb_to_rgba(buffer)), false),
            BitmapBuffer::Rgba(buffer) => (Cow::Borrowed(buffer), true),
        };

        Self {
            multicolor,
            buffer,
            width: glyph.width,
            height: glyph.height,
            top: glyph.top,
            left: glyph.left,
            advance: glyph.advance,
        }
    }
}

/// Key for caching atlas entries.
#[derive(Copy, Clone, Hash, PartialEq, Eq)]
enum CacheKey {
    Character(char),
    Svg(Svg),
}

impl From<char> for CacheKey {
    fn from(c: char) -> Self {
        Self::Character(c)
    }
}

impl From<Svg> for CacheKey {
    fn from(svg: Svg) -> Self {
        Self::Svg(svg)
    }
}

/// Built-in SVGs.
#[derive(Copy, Clone, Hash, PartialEq, Eq)]
pub enum Svg {
    BatteryCharging100,
    BatteryCharging80,
    BatteryCharging60,
    BatteryCharging40,
    BatteryCharging20,
    Battery100,
    Battery80,
    Battery60,
    Battery40,
    Battery20,
}

impl Svg {
    /// Get SVG's dimensions.
    pub const fn size(&self) -> (u32, u32) {
        match self {
            Self::BatteryCharging100 => (22, 14),
            Self::BatteryCharging80 => (22, 14),
            Self::BatteryCharging60 => (22, 14),
            Self::BatteryCharging40 => (22, 14),
            Self::BatteryCharging20 => (22, 14),
            Self::Battery100 => (22, 8),
            Self::Battery80 => (22, 8),
            Self::Battery60 => (22, 8),
            Self::Battery40 => (22, 8),
            Self::Battery20 => (22, 8),
        }
    }

    /// Get SVG's text content.
    const fn content(&self) -> &'static str {
        match self {
            Self::BatteryCharging100 => include_str!("../svgs/battery_charging_100.svg"),
            Self::BatteryCharging80 => include_str!("../svgs/battery_charging_80.svg"),
            Self::BatteryCharging60 => include_str!("../svgs/battery_charging_60.svg"),
            Self::BatteryCharging40 => include_str!("../svgs/battery_charging_40.svg"),
            Self::BatteryCharging20 => include_str!("../svgs/battery_charging_20.svg"),
            Self::Battery100 => include_str!("../svgs/battery_100.svg"),
            Self::Battery80 => include_str!("../svgs/battery_80.svg"),
            Self::Battery60 => include_str!("../svgs/battery_60.svg"),
            Self::Battery40 => include_str!("../svgs/battery_40.svg"),
            Self::Battery20 => include_str!("../svgs/battery_20.svg"),
        }
    }
}
