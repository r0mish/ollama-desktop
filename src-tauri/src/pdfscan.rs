//! PDF scannés : extraction des images de page pour les faire lire par un modèle vision.
//! Un PDF scanné contient en général une grande image par page ; elle est reprise telle
//! quelle (JPEG) ou convertie en PNG (pixels bruts), sans moteur de rendu PDF.

use base64::Engine;
use lopdf::Document;
use serde::Serialize;

type R<T> = Result<T, String>;

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageImages {
    /// Une image (data URL) par page lisible, dans l'ordre.
    pub images: Vec<String>,
    /// Nombre de pages du document.
    pub pages: usize,
    /// Pages sans image exploitable (format d'image non pris en charge, page vide).
    pub skipped: usize,
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Pixels bruts 8 bits (gris ou RVB) vers PNG.
fn to_png(width: u32, height: u32, channels: usize, pixels: &[u8]) -> Option<Vec<u8>> {
    if pixels.len() < width as usize * height as usize * channels {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(if channels == 1 { png::ColorType::Grayscale } else { png::ColorType::Rgb });
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&pixels[..width as usize * height as usize * channels]).ok()?;
    }
    Some(out)
}

/// Plus grande image d'une page, encodée pour un modèle vision.
fn page_image(doc: &Document, page_id: lopdf::ObjectId) -> Option<String> {
    let images = doc.get_page_images(page_id).ok()?;
    let img = images.iter().filter(|i| i.width > 0 && i.height > 0).max_by_key(|i| i.width * i.height)?;
    // Trop petite pour être une page scannée (logo, pictogramme).
    if img.width * img.height < 200 * 200 {
        return None;
    }
    let filters: Vec<&str> = img.filters.as_deref().unwrap_or(&[]).iter().map(String::as_str).collect();
    match filters.as_slice() {
        ["DCTDecode"] => Some(data_url("image/jpeg", img.content)),
        [] | ["FlateDecode"] if img.bits_per_component == Some(8) => {
            let channels = match img.color_space.as_deref() {
                Some("DeviceGray") => 1,
                Some("DeviceRGB") => 3,
                _ => return None,
            };
            let pixels = if filters.is_empty() { img.content.to_vec() } else { doc.get_object(img.id).ok()?.as_stream().ok()?.decompressed_content().ok()? };
            to_png(img.width as u32, img.height as u32, channels, &pixels).map(|png| data_url("image/png", &png))
        }
        // CCITT, JBIG2, JPEG 2000… : non pris en charge.
        _ => None,
    }
}

/// Images des `max_pages` premières pages d'un PDF scanné.
pub fn page_images(bytes: &[u8], max_pages: usize) -> R<PageImages> {
    let doc = Document::load_mem(bytes).map_err(|e| format!("PDF illisible : {e}"))?;
    let pages = doc.get_pages();
    let mut out = PageImages { pages: pages.len(), ..Default::default() };
    for (_, &id) in pages.iter().take(max_pages) {
        match page_image(&doc, id) {
            Some(url) => out.images.push(url),
            None => out.skipped += 1,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object, Stream};

    /// PDF d'une page par image, chaque image posée en XObject.
    fn pdf_with(images: Vec<Stream>) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for img in images {
            let img_id = doc.add_object(img);
            let content = doc.add_object(Stream::new(dictionary! {}, b"q 595 0 0 842 0 0 cm /Im0 Do Q".to_vec()));
            let page = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id, "Contents" => content,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
            });
            kids.push(Object::Reference(page));
        }
        let count = kids.len() as i64;
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn image(width: i64, height: i64, color: &str, filter: Option<&str>, content: Vec<u8>) -> Stream {
        let mut dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => width, "Height" => height,
            "ColorSpace" => color, "BitsPerComponent" => 8,
        };
        if let Some(f) = filter {
            dict.set("Filter", f);
        }
        Stream::new(dict, content)
    }

    #[test]
    fn scanned_pages_become_images() {
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
        let mut flate = image(300, 300, "DeviceGray", None, vec![128u8; 300 * 300]);
        flate.compress().unwrap();
        let pdf = pdf_with(vec![
            image(1000, 1400, "DeviceRGB", Some("DCTDecode"), jpeg.clone()),
            flate,
            image(1000, 1400, "DeviceGray", Some("JBIG2Decode"), vec![0; 16]),
            image(20, 20, "DeviceRGB", Some("DCTDecode"), jpeg.clone()),
        ]);
        let out = page_images(&pdf, 10).unwrap();
        assert_eq!((out.pages, out.images.len(), out.skipped), (4, 2, 2));
        // JPEG repris tel quel ; pixels bruts convertis en PNG valide.
        assert_eq!(out.images[0], data_url("image/jpeg", &jpeg));
        let png_bytes = base64::engine::general_purpose::STANDARD.decode(out.images[1].strip_prefix("data:image/png;base64,").unwrap()).unwrap();
        let info = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().unwrap().info().clone();
        assert_eq!((info.width, info.height), (300, 300));
        // Limite de pages respectée.
        assert_eq!(page_images(&pdf, 1).unwrap().images.len(), 1);
        assert!(page_images(b"pas un pdf", 5).is_err());
    }
}
