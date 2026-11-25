use std::{path::Path, sync::Arc};

use anyhow::{Result, ensure};
use image::RgbaImage;

use crate::DiskImage;

#[derive(Clone)]
pub enum ImageSource {
    Memory(Arc<RgbaImage>),
    Disk(Arc<DiskImage>),
}

impl ImageSource {
    pub fn width(&self) -> u32 {
        match self {
            Self::Memory(image) => image.width(),
            Self::Disk(image) => image.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Memory(image) => image.height(),
            Self::Disk(image) => image.height(),
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width(), self.height())
    }

    pub fn is_disk(&self) -> bool {
        matches!(self, Self::Disk(_))
    }

    pub fn memory(&self) -> Option<&Arc<RgbaImage>> {
        match self {
            Self::Memory(image) => Some(image),
            Self::Disk(_) => None,
        }
    }

    pub fn read_rows(&self, start: u32, rows: u32) -> Result<RgbaImage> {
        match self {
            Self::Memory(image) => {
                ensure!(rows > 0, "image row read is empty");
                ensure!(
                    start
                        .checked_add(rows)
                        .is_some_and(|end| end <= image.height()),
                    "image row read is out of bounds"
                );
                Ok(
                    image::imageops::crop_imm(image.as_ref(), 0, start, image.width(), rows)
                        .to_image(),
                )
            }
            Self::Disk(image) => image.read_rows(start, rows),
        }
    }

    pub fn thumbnail(&self, max_width: u32, max_height: u32) -> Result<RgbaImage> {
        match self {
            Self::Memory(image) => Ok(image::imageops::thumbnail(
                image.as_ref(),
                max_width.max(1),
                max_height.max(1),
            )),
            Self::Disk(image) => image.thumbnail(max_width, max_height),
        }
    }

    pub fn encode_png(&self, path: &Path) -> Result<()> {
        match self {
            Self::Memory(image) => crate::save_png(image, path),
            Self::Disk(image) => image.encode_png(path),
        }
    }
}

impl From<Arc<ImageSource>> for ImageSource {
    fn from(image: Arc<ImageSource>) -> Self {
        (*image).clone()
    }
}

impl From<RgbaImage> for ImageSource {
    fn from(image: RgbaImage) -> Self {
        Self::Memory(Arc::new(image))
    }
}

impl From<Arc<RgbaImage>> for ImageSource {
    fn from(image: Arc<RgbaImage>) -> Self {
        Self::Memory(image)
    }
}

impl From<DiskImage> for ImageSource {
    fn from(image: DiskImage) -> Self {
        Self::Disk(Arc::new(image))
    }
}

impl From<Arc<DiskImage>> for ImageSource {
    fn from(image: Arc<DiskImage>) -> Self {
        Self::Disk(image)
    }
}
