use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};

use failure::Error;
use image::png::PngDecoder;
use image::{
    self, ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageLuma8,
};
use memmap::Mmap;
use zip::ZipArchive;

use crate::cache::{AssetLoadContext, GeneratedAsset, WebAsset};
use crate::terrain::raster::{BitContainer, GlobalRaster, MMappedRasterHeader, Raster, RasterSource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LandCoverKind {
    TreeCover,
    WaterMask,
}
impl RasterSource for LandCoverKind {
    type Type = u8;
    type Container = Vec<u8>;
    fn bands(&self) -> usize {
        1
    }
    fn load(
        &self,
        context: &mut AssetLoadContext,
        latitude: i16,
        longitude: i16,
    ) -> Option<Raster<u8>> {
        Some(
            LandCoverParams {
                latitude,
                longitude,
                kind: *self,
                raw: None,
            }.load(context)
            .unwrap(),
        )
    }
}

/// Coordinates are of the lower left corner of the 10x10 degree cell.
#[derive(Debug, Eq, PartialEq)]
struct RawLandCoverParams {
    pub latitude: i16,
    pub longitude: i16,
    pub kind: LandCoverKind,
}
impl WebAsset for RawLandCoverParams {
    type Type = Arc<Mutex<DynamicImage>>;

    fn url(&self) -> String {
        let (latitude, longitude) = (self.latitude + 10, self.longitude);
        assert_eq!(latitude % 10, 0);
        assert_eq!(longitude % 10, 0);
        let n_or_s = if latitude >= 0 { 'N' } else { 'S' };
        let e_or_w = if longitude >= 0 { 'E' } else { 'W' };

        match self.kind {
            LandCoverKind::TreeCover => format!(
                "https://edcintl.cr.usgs.gov/downloads/sciweb1/shared/gtc/downloads/\
                 treecover2010_v3_individual/{:02}{}_{:03}{}_treecover2010_v3.tif.zip",
                latitude.abs(),
                n_or_s,
                longitude.abs(),
                e_or_w,
            ),
            LandCoverKind::WaterMask => format!(
                "https://edcintl.cr.usgs.gov/downloads/sciweb1/shared/gtc/downloads/\
                 WaterMask2010_UMD_individual/Hansen_GFC2013_datamask_{:02}{}_{:03}{}\
                 .tif.zip",
                latitude.abs(),
                n_or_s,
                longitude.abs(),
                e_or_w,
            ),
        }
    }
    fn filename(&self) -> String {
        let (latitude, longitude) = (self.latitude + 10, self.longitude);
        assert_eq!(latitude % 10, 0);
        assert_eq!(longitude % 10, 0);
        let n_or_s = if latitude >= 0 { 'N' } else { 'S' };
        let e_or_w = if longitude >= 0 { 'E' } else { 'W' };

        match self.kind {
            LandCoverKind::TreeCover => format!(
                "treecover/raw/{:02}{}_{:03}{}_treecover2010_v3.tif.zip",
                latitude.abs(),
                n_or_s,
                longitude.abs(),
                e_or_w,
            ),
            LandCoverKind::WaterMask => format!(
                "watermask/raw/Hansen_GFC2013_datamask_{:02}{}_{:03}{}.tif.zip",
                latitude.abs(),
                n_or_s,
                longitude.abs(),
                e_or_w,
            ),
        }
    }
    fn parse(&self, context: &mut AssetLoadContext, data: Vec<u8>) -> Result<Self::Type, Error> {
        let mut zip = ZipArchive::new(Cursor::new(data))?;
        assert_eq!(zip.len(), 1);

        let mut data = Vec::new();
        zip.by_index(0)?.read_to_end(&mut data)?;

        let image = image::load_from_memory_with_format(&data[..], ImageFormat::Tiff)?;
        let image = Arc::new(Mutex::new(image));

        for latitude in self.latitude..(self.latitude + 10) {
            for longitude in self.longitude..(self.longitude + 10) {
                let params = LandCoverParams {
                    latitude,
                    longitude,
                    kind: self.kind,
                    raw: Some(image.clone()),
                };
                assert_eq!(params.raw_params(), *self);
                params.load(context)?;
            }
        }

        Ok(image)
    }
}

pub struct LandCoverParams {
    pub latitude: i16,
    pub longitude: i16,
    pub kind: LandCoverKind,
    pub raw: Option<Arc<Mutex<DynamicImage>>>,
}
impl LandCoverParams {
    fn raw_params(&self) -> RawLandCoverParams {
        RawLandCoverParams {
            latitude: self.latitude - ((self.latitude % 10) + 10) % 10,
            longitude: self.longitude - ((self.longitude % 10) + 10) % 10,
            kind: self.kind,
        }
    }

    fn generate_from_raw(&self) -> Result<Raster<u8>, Error> {
        let (w, h, image) = {
            let mut image = self.raw.as_ref().unwrap().lock().unwrap();
            let (w, h) = (image.width(), image.height());
            assert_eq!(w, h);
            let image = image
                .crop(
                    (w / 10) * ((self.longitude % 10 + 10) % 10) as u32,
                    (h / 10) * (9 - ((self.latitude % 10 + 10) % 10) as u32),
                    w / 10 + 1,
                    h / 10 + 1,
                ).clone();
            (w, h, image)
        };
        // let image = image.rotate90().flipv();
        let values = if let ImageLuma8(image) = image {
            image.into_raw().into_iter()
        } else {
            unreachable!()
        };

        let values = match self.kind {
            LandCoverKind::TreeCover => values.collect(),
            LandCoverKind::WaterMask => values
                .map(|v| {
                    if v == 1 {
                        0
                    } else if v == 0 || v == 2 {
                        255
                    } else {
                        unreachable!()
                    }
                }).collect(),
        };

        Ok(Raster {
            width: w as usize / 10 + 1,
            height: h as usize / 10 + 1,
            bands: 1,
            cell_size: 10.0 / (w - 1) as f64,
            longitude_llcorner: self.longitude as f64,
            latitude_llcorner: self.latitude as f64,
            values,
        })
    }
}
impl GeneratedAsset for LandCoverParams {
    type Type = Raster<u8>;

    fn filename(&self) -> String {
        let n_or_s = if self.latitude >= 0 { 'N' } else { 'S' };
        let e_or_w = if self.longitude >= 0 { 'E' } else { 'W' };

        let directory = match self.kind {
            LandCoverKind::TreeCover => "treecover/processed",
            LandCoverKind::WaterMask => "watermask/processed",
        };

        format!(
            "{}/{:02}{}_{:03}{}.raster",
            directory,
            self.latitude.abs(),
            n_or_s,
            self.longitude.abs(),
            e_or_w,
        )
    }

    fn generate(&self, context: &mut AssetLoadContext) -> Result<Self::Type, Error> {
        if self.raw.is_some() {
            return self.generate_from_raw();
        }

        Self {
            latitude: self.latitude,
            longitude: self.longitude,
            kind: self.kind,
            raw: Some(self.raw_params().load(context)?),
        }.generate_from_raw()
    }
}

pub struct BlueMarble;
impl WebAsset for BlueMarble {
    type Type = GlobalRaster<u8>;

    fn url(&self) -> String {
        "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/\
         world.200406.3x21600x10800.png"
            .to_owned()
    }
    fn filename(&self) -> String {
        "bluemarble/world.200406.3x21600x10800.png".to_owned()
    }
    fn parse(&self, context: &mut AssetLoadContext, data: Vec<u8>) -> Result<Self::Type, Error> {
        let mut decoder = PngDecoder::new(Cursor::new(data))?;
        let (width, height) = decoder.dimensions();
        let (width, height) = (width as usize, height as usize);
        assert_eq!(decoder.color_type(), ColorType::Rgb8);

        context.set_progress_and_total(0, height / 108);
        let row_len = width * 3;
        let mut values = vec![0; decoder.total_bytes() as usize];
        let mut reader = decoder.into_reader()?;
        for row in 0..height {
            reader.read_exact(&mut values[(row * row_len)..((row + 1) * row_len)])?;
            if (row + 1) % 108 == 0 {
                context.set_progress((row + 1) / 108);
            }
        }

        Ok(GlobalRaster {
            width,
            height,
            bands: 3,
            values,
        })
    }
}

pub struct BlueMarbleTile {
    latitude_llcorner: i16,
    longitude_llcorner: i16,
}
impl BlueMarbleTile {
    fn name(&self) -> String {
        let x = match self.longitude_llcorner {
            -180 => "A",
            -90 => "B",
            0 => "C",
            90 => "D",
            _ => unreachable!(),
        };
        let y = match self.latitude_llcorner {
            0 => "1",
            -90 => "2",
            _ => unreachable!(),
        };
        format!("world.200406.3x21600x21600.{}{}.png", x, y)
    }
}
impl WebAsset for BlueMarbleTile {
    type Type = (MMappedRasterHeader, Vec<u8>);

    fn url(&self) -> String {
        format!(
            "https://eoimages.gsfc.nasa.gov/images/imagerecords/76000/76487/{}",
            self.name()
        )
    }
    fn filename(&self) -> String {
        format!("bluemarble/{}", self.name())
    }
    fn parse(&self, context: &mut AssetLoadContext, data: Vec<u8>) -> Result<Self::Type, Error> {
        let mut decoder = PngDecoder::new(Cursor::new(data))?;
        let (width, height) = decoder.dimensions();
        let (width, height) = (width as usize, height as usize);
        assert_eq!(decoder.color_type(), ColorType::Rgb8);

        context.set_progress_and_total(0, height / 108);
        let row_len = width * 3 * 12;
        let mut values = vec![0; decoder.total_bytes() as usize];
        let mut reader = decoder.into_reader()?;
        for row in 0..height {
            reader.read_exact(&mut values[(row * row_len)..((row + 1) * row_len)])?;
            if (row + 1) % 108 == 0 {
                context.set_progress((row + 1) / 108);
            }
        }

        Ok((
            MMappedRasterHeader {
                width,
                height,
                bands: 3,
                cell_size: 90.0 / 21600.0,
                latitude_llcorner: self.latitude_llcorner as f64,
                longitude_llcorner: self.longitude_llcorner as f64,
            },
            values,
        ))
    }
}

pub struct BlueMarbleTileSource;
impl RasterSource for BlueMarbleTileSource {
    type Type = u8;
    type Container = Mmap;
    fn bands(&self) -> usize {
        3
    }
    fn raster_size(&self) -> i16 {
        90
    }
    fn load(
        &self,
        context: &mut AssetLoadContext,
        latitude: i16,
        longitude: i16,
    ) -> Option<Raster<Self::Type, Self::Container>> {
        Some(
            Raster::from_mmapped_raster(
                BlueMarbleTile {
                    latitude_llcorner: latitude,
                    longitude_llcorner: longitude,
                },
                context,
            ).unwrap(),
        )
    }
}

pub struct GlobalWaterMask;
impl WebAsset for GlobalWaterMask {
    type Type = GlobalRaster<u8, BitContainer>;

    fn url(&self) -> String {
        "https://landcover.usgs.gov/documents/GlobalLandCover_tif.zip".to_owned()
    }
    fn filename(&self) -> String {
        "watermask/GlobalLandCover_tif.zip".to_owned()
    }
    fn parse(&self, context: &mut AssetLoadContext, data: Vec<u8>) -> Result<Self::Type, Error> {
        context.set_progress_and_total(0, 100);
        let mut zip = ZipArchive::new(Cursor::new(data))?;
        assert_eq!(zip.len(), 1);

        let mut data = Vec::new();
        zip.by_index(0)?.read_to_end(&mut data)?;

        let image = image::load_from_memory_with_format(&data[..], ImageFormat::Tiff)?;
        context.set_progress(100);
        let (width, height) = image.dimensions();
        let (width, height) = (width as usize, height as usize);
        if let DynamicImage::ImageLuma8(image) = image {
            Ok(GlobalRaster {
                width,
                height,
                bands: 1,
                values: BitContainer(image.into_raw().into_iter().map(|v| v == 0).collect()),
            })
        } else {
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_raw_params() {
        assert_eq!(
            LandCoverParams {
                latitude: 165,
                longitude: 31,
                kind: LandCoverKind::TreeCover,
                raw: None,
            }.raw_params(),
            RawLandCoverParams {
                latitude: 160,
                longitude: 30,
                kind: LandCoverKind::TreeCover,
            }
        );

        assert_eq!(
            LandCoverParams {
                latitude: 20,
                longitude: 20,
                kind: LandCoverKind::TreeCover,
                raw: None,
            }.raw_params(),
            RawLandCoverParams {
                latitude: 20,
                longitude: 20,
                kind: LandCoverKind::TreeCover,
            }
        );

        assert_eq!(
            LandCoverParams {
                latitude: -18,
                longitude: -18,
                kind: LandCoverKind::TreeCover,
                raw: None,
            }.raw_params(),
            RawLandCoverParams {
                latitude: -20,
                longitude: -20,
                kind: LandCoverKind::TreeCover,
            }
        );

        assert_eq!(
            LandCoverParams {
                latitude: -30,
                longitude: -30,
                kind: LandCoverKind::TreeCover,
                raw: None,
            }.raw_params(),
            RawLandCoverParams {
                latitude: -30,
                longitude: -30,
                kind: LandCoverKind::TreeCover,
            }
        );
    }
}
