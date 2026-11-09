//! Versioned private authoring types. Canonical tokens are not display labels.
use crate::{
    images::ImageSha,
    models::{ImageConfidence, ImageCrop, ShopSeason, Usage},
};
use icu_normalizer::ComposingNormalizer;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, num::NonZeroU32, str::FromStr};

macro_rules! text_type {
    ($name:ident,$limit:expr,$predicate:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                let value = ComposingNormalizer::new_nfc()
                    .normalize(&value)
                    .into_owned();
                if value.len() <= $limit && ($predicate)(&value) {
                    Ok(Self(value))
                } else {
                    Err("invalid_source_text")
                }
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
fn text(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}
fn note(value: &str) -> bool {
    !value.trim().is_empty()
        && !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}
fn category(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
}
text_type!(NfcText, 1024, text);
text_type!(ComponentId, 200, token);
text_type!(PrivateNote, 4096, note);
text_type!(MakerSku, 1024, text);
text_type!(MakerVariantCode, 1024, text);
text_type!(BrandId, 200, token);
text_type!(StyleId, 200, token);
text_type!(VariantId, 200, token);
text_type!(SizingProfileId, 200, token);
text_type!(InventoryId, 200, crate::models::valid_id);
text_type!(FittingId, 200, crate::models::valid_id);
text_type!(CategoryId, 200, category);
text_type!(Currency, 3, |s: &str| s.len() == 3
    && s.bytes().all(|b| b.is_ascii_uppercase()));
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HttpsUrl(String);
impl TryFrom<String> for HttpsUrl {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !crate::models::valid_https_url(&value) {
            return Err("invalid_https_url");
        }
        Ok(Self(value))
    }
}
impl From<HttpsUrl> for String {
    fn from(value: HttpsUrl) -> Self {
        value.0
    }
}
impl HttpsUrl {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

macro_rules! choices {
    ($name:ident { $($variant:ident => $token:literal => $display:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $token)] $variant),+
        }
        impl $name {
            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $display),+ }
            }
            pub fn token(self) -> &'static str {
                match self { $(Self::$variant => $token),+ }
            }
        }
        impl FromStr for $name {
            type Err = &'static str;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($token => Ok(Self::$variant),)+
                    _ => Err("unsupported_source_token"),
                }
            }
        }
    };
}

choices!(ColorFamily {
    Black => "black" => "Black",
    White => "white" => "White",
    OffWhite => "off_white" => "Off-white",
    Ivory => "ivory" => "Ivory",
    Cream => "cream" => "Cream",
    Gray => "gray" => "Gray",
    Beige => "beige" => "Beige",
    Taupe => "taupe" => "Taupe",
    Brown => "brown" => "Brown",
    Blue => "blue" => "Blue",
    Navy => "navy" => "Navy",
    Green => "green" => "Green",
    Red => "red" => "Red",
    Pink => "pink" => "Pink",
    Purple => "purple" => "Purple",
    Yellow => "yellow" => "Yellow",
    Orange => "orange" => "Orange",
    Gold => "gold" => "Gold",
    Silver => "silver" => "Silver",
    Clear => "clear" => "Clear",
    Multicolor => "multicolor" => "Multicolor",
});
choices!(ColorDisplay {
    Families => "families" => "Families",
    MakerShade => "maker_shade" => "Maker shade",
});
choices!(AlphaSize {
    Xxxs => "xxxs" => "XXXS",
    Xxs => "xxs" => "XXS",
    Xs => "xs" => "XS",
    S => "s" => "S",
    M => "m" => "M",
    L => "l" => "L",
    Xl => "xl" => "XL",
    Xxl => "xxl" => "XXL",
    Xxxl => "xxxl" => "XXXL",
    FourXl => "4xl" => "4XL",
    FiveXl => "5xl" => "5XL",
});
choices!(SizingSystem {
    Native => "native" => "",
    Us => "us" => "US",
    Uk => "uk" => "UK",
    Fr => "fr" => "FR",
    It => "it" => "IT",
    Eu => "eu" => "EU",
    Au => "au" => "AU",
});
choices!(SizeKind {
    Clothing => "clothing" => "Clothing",
    Footwear => "footwear" => "Footwear",
    Denim => "denim" => "Denim",
    Belt => "belt" => "Belt",
    Eyewear => "eyewear" => "Eyewear",
    Accessory => "accessory" => "Accessory",
});
choices!(SizeScope {
    Men => "men" => "Men's",
    Women => "women" => "Women's",
    Unisex => "unisex" => "Unisex",
});
choices!(SizeForm {
    Alpha => "alpha" => "Alpha",
    AlphaPair => "alpha_pair" => "Alpha pair",
    Numeric => "numeric" => "Numeric",
    NumericPair => "numeric_pair" => "Numeric pair",
    Petite => "petite" => "Petite",
    Denim => "denim" => "Denim",
    OneSize => "one_size" => "One size",
    Eyewear => "eyewear" => "Eyewear",
});
choices!(Fiber {
    Cotton => "cotton" => "Cotton",
    Linen => "linen" => "Linen",
    Silk => "silk" => "Silk",
    Wool => "wool" => "Wool",
    Cashmere => "cashmere" => "Cashmere",
    Mohair => "mohair" => "Mohair",
    CamelHair => "camel_hair" => "Camel hair",
    Alpaca => "alpaca" => "Alpaca",
    Angora => "angora" => "Angora",
    Viscose => "viscose" => "Viscose",
    Modal => "modal" => "Modal",
    Lyocell => "lyocell" => "Lyocell",
    Cupro => "cupro" => "Cupro",
    RegeneratedCellulose => "regenerated_cellulose" => "Regenerated cellulose",
    Acetate => "acetate" => "Acetate",
    Triacetate => "triacetate" => "Triacetate",
    Polyester => "polyester" => "Polyester",
    Polyamide => "polyamide" => "Polyamide",
    Nylon => "nylon" => "Nylon",
    Elastane => "elastane" => "Elastane",
    Polyurethane => "polyurethane" => "Polyurethane",
    Polypropylene => "polypropylene" => "Polypropylene",
    Acrylic => "acrylic" => "Acrylic",
    Hemp => "hemp" => "Hemp",
    Raffia => "raffia" => "Raffia",
    Sisal => "sisal" => "Sisal",
    Paper => "paper" => "Paper",
});
impl Fiber {
    pub fn from_legacy(value: &str) -> Result<Self, &'static str> {
        if value.eq_ignore_ascii_case("spandex") {
            Ok(Self::Elastane)
        } else {
            value.to_ascii_lowercase().parse()
        }
    }
}
choices!(FiberQualifier {
    Organic => "organic" => "Organic",
    Recycled => "recycled" => "Recycled",
    Virgin => "virgin" => "Virgin",
    Merino => "merino" => "Merino",
    Lambswool => "lambswool" => "Lambswool",
    Pima => "pima" => "Pima",
    Regenerative => "regenerative" => "Regenerative",
});
choices!(MaterialPart {
    Main => "main" => "Main",
    Shell => "shell" => "Shell",
    Lining => "lining" => "Lining",
    Fill => "fill" => "Fill",
    Trim => "trim" => "Trim",
    Upper => "upper" => "Upper",
    Sole => "sole" => "Sole",
    Outsole => "outsole" => "Outsole",
    Insole => "insole" => "Insole",
    Ribbing => "ribbing" => "Ribbing",
    Contrast => "contrast" => "Contrast",
    Lace => "lace" => "Lace",
    Crochet => "crochet" => "Crochet",
    Collar => "collar" => "Collar",
    Cuffs => "cuffs" => "Cuffs",
    Stitching => "stitching" => "Stitching",
    Frame => "frame" => "Frame",
    Temples => "temples" => "Temples",
    Lenses => "lenses" => "Lenses",
    Embroidery => "embroidery" => "Embroidery",
});
choices!(Construction {
    Denim => "denim" => "Denim",
    Satin => "satin" => "Satin",
    Terry => "terry" => "Terry",
    Jersey => "jersey" => "Jersey",
    Knit => "knit" => "Knit",
    Woven => "woven" => "Woven",
    Fleece => "fleece" => "Fleece",
    Twill => "twill" => "Twill",
    Corduroy => "corduroy" => "Corduroy",
    Poplin => "poplin" => "Poplin",
    Canvas => "canvas" => "Canvas",
    Tulle => "tulle" => "Tulle",
    Lace => "lace" => "Lace",
    Crochet => "crochet" => "Crochet",
    Ribbed => "ribbed" => "Ribbed",
    RibbedJersey => "ribbed_jersey" => "Ribbed jersey",
    Tweed => "tweed" => "Tweed",
    Velvet => "velvet" => "Velvet",
    Crepe => "crepe" => "Crepe",
    CrepeDeChine => "crepe_de_chine" => "Crepe de chine",
    Organza => "organza" => "Organza",
    Chiffon => "chiffon" => "Chiffon",
    Mesh => "mesh" => "Mesh",
    Gabardine => "gabardine" => "Gabardine",
    Voile => "voile" => "Voile",
    Jacquard => "jacquard" => "Jacquard",
    FlakeJacquard => "flake_jacquard" => "Flake jacquard",
    Grosgrain => "grosgrain" => "Grosgrain",
    Oxford => "oxford" => "Oxford",
    Boucle => "boucle" => "Bouclé",
    Textile => "textile" => "Textile",
});
choices!(LeatherAnimal {
    Bovine => "bovine" => "Bovine",
    Calf => "calf" => "Calfskin",
    Lamb => "lamb" => "Lambskin",
    Sheep => "sheep" => "Sheepskin",
    Goat => "goat" => "Goatskin",
    Pig => "pig" => "Pigskin",
    Deer => "deer" => "Deerskin",
    Horse => "horse" => "Horsehide",
    Buffalo => "buffalo" => "Buffalo",
    Snake => "snake" => "Snakeskin",
    Crocodile => "crocodile" => "Crocodile",
    Alligator => "alligator" => "Alligator",
    Ostrich => "ostrich" => "Ostrich",
    Lizard => "lizard" => "Lizard",
});
choices!(Finish {
    Smooth => "smooth" => "Smooth",
    Suede => "suede" => "Suede",
    WaxedSuede => "waxed_suede" => "Waxed suede",
    Nubuck => "nubuck" => "Nubuck",
    Patent => "patent" => "Patent",
    Grained => "grained" => "Grained",
    Pebbled => "pebbled" => "Pebbled",
    Embossed => "embossed" => "Embossed",
    Nappa => "nappa" => "Nappa",
    Box => "box" => "Box",
    Saffiano => "saffiano" => "Saffiano",
    HairOn => "hair_on" => "Hair-on",
    Shearling => "shearling" => "Shearling",
});
choices!(Metal {
    Steel => "steel" => "Steel",
    Brass => "brass" => "Brass",
    Aluminium => "aluminium" => "Aluminium",
    Silver => "silver" => "Silver",
    Gold => "gold" => "Gold",
    Platinum => "platinum" => "Platinum",
    Zinc => "zinc" => "Zinc",
    Copper => "copper" => "Copper",
    Titanium => "titanium" => "Titanium",
});
choices!(AnimalFill {
    Down => "down" => "Down",
    Feather => "feather" => "Feather",
    GooseDown => "goose_down" => "Goose down",
    GooseFeather => "goose_feather" => "Goose feather",
    DuckDown => "duck_down" => "Duck down",
    DuckFeather => "duck_feather" => "Duck feather",
});
choices!(Polymer {
    Acetate => "acetate" => "Acetate",
    Polyamide => "polyamide" => "Polyamide",
    Nylon => "nylon" => "Nylon",
    Polyurethane => "polyurethane" => "Polyurethane",
    Polypropylene => "polypropylene" => "Polypropylene",
    Polyester => "polyester" => "Polyester",
    Acrylic => "acrylic" => "Acrylic",
    Resin => "resin" => "Resin",
    Plastic => "plastic" => "Plastic",
    Pvc => "pvc" => "PVC",
    Eva => "eva" => "EVA",
});
choices!(MaterialProperty {
    Stretch => "stretch" => "Stretch",
    HeavyDuty => "heavy_duty" => "Heavy-duty",
});
choices!(WeightUnit {
    Ounce => "oz" => "oz",
    GramsPerSquareMetre => "gsm" => "gsm",
});
choices!(TradeMaterialClaim {
    Circulose => "circulose" => "Circulose",
    PyratexCirculose => "pyratex_circulose" => "PYRATEX® Circulose",
});
choices!(ImageKind {
    OfficialProductReference => "official_product_reference" => "Official product reference",
    RetailerProductReference => "retailer_product_reference" => "Retailer product reference",
    ProductReference => "product_reference" => "Product reference",
    SourcePhoto => "source_photo" => "Source photo",
    MerchantProductImage => "merchant_product_image" => "Merchant product image",
    MerchantEmailProductReference => "merchant_email_product_reference" => "Merchant email product reference",
    UserConfirmedModelReference => "user_confirmed_model_reference" => "User-confirmed model reference",
    UserConfirmedCatalogReference => "user_confirmed_catalog_reference" => "User-confirmed catalog reference",
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<T>",
    into = "Vec<T>",
    bound(
        serialize = "T: Serialize + Ord + Clone",
        deserialize = "T: Deserialize<'de> + Ord + Clone"
    )
)]
pub struct NonEmptySet<T: Ord + Clone>(BTreeSet<T>);
impl<T: Ord + Clone> TryFrom<Vec<T>> for NonEmptySet<T> {
    type Error = &'static str;
    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        let length = value.len();
        let set: BTreeSet<_> = value.into_iter().collect();
        if length == 0 || set.len() != length {
            Err("empty_or_duplicate_set")
        } else {
            Ok(Self(set))
        }
    }
}
impl<T: Ord + Clone> From<NonEmptySet<T>> for Vec<T> {
    fn from(value: NonEmptySet<T>) -> Self {
        value.0.into_iter().collect()
    }
}
impl<T: Ord + Clone> NonEmptySet<T> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "Vec<FiberQualifier>", into = "Vec<FiberQualifier>")]
pub struct Qualifiers(BTreeSet<FiberQualifier>);
impl TryFrom<Vec<FiberQualifier>> for Qualifiers {
    type Error = &'static str;
    fn try_from(v: Vec<FiberQualifier>) -> Result<Self, Self::Error> {
        let n = v.len();
        let s = v.into_iter().collect::<BTreeSet<_>>();
        if s.len() != n {
            Err("duplicate_qualifier")
        } else {
            Ok(Self(s))
        }
    }
}
impl From<Qualifiers> for Vec<FiberQualifier> {
    fn from(value: Qualifiers) -> Self {
        value.0.into_iter().collect()
    }
}
impl Qualifiers {
    pub fn iter(&self) -> impl Iterator<Item = &FiberQualifier> {
        self.0.iter()
    }
    pub fn contains(&self, q: FiberQualifier) -> bool {
        self.0.contains(&q)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum NativeNumber {
    DoubleZero,
    Whole(u16),
    Half(u16),
    OneThird(u16),
    TwoThirds(u16),
}
impl NativeNumber {
    pub fn sixths(&self) -> u32 {
        match self {
            Self::DoubleZero => 0,
            Self::Whole(n) => u32::from(*n) * 6,
            Self::Half(n) => u32::from(*n) * 6 + 3,
            Self::OneThird(n) => u32::from(*n) * 6 + 2,
            Self::TwoThirds(n) => u32::from(*n) * 6 + 4,
        }
    }
}
impl TryFrom<String> for NativeNumber {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s == "00" {
            return Ok(Self::DoubleZero);
        }
        let (n, fraction) = if let Some(n) = s.strip_suffix(" 1/3") {
            (n, 2)
        } else if let Some(n) = s.strip_suffix(" 2/3") {
            (n, 4)
        } else if let Some(n) = s.strip_suffix(".5") {
            (n, 3)
        } else {
            (s.as_str(), 0)
        };
        let value = n.parse::<u16>().map_err(|_| "invalid_native_number")?;
        if value > 1000 || n != value.to_string() {
            return Err("invalid_native_number");
        }
        Ok(match fraction {
            2 => Self::OneThird(value),
            4 => Self::TwoThirds(value),
            3 => Self::Half(value),
            _ => Self::Whole(value),
        })
    }
}
impl From<NativeNumber> for String {
    fn from(value: NativeNumber) -> Self {
        match value {
            NativeNumber::DoubleZero => "00".into(),
            NativeNumber::Whole(n) => n.to_string(),
            NativeNumber::Half(n) => format!("{n}.5"),
            NativeNumber::OneThird(n) => format!("{n} 1/3"),
            NativeNumber::TwoThirds(n) => format!("{n} 2/3"),
        }
    }
}
impl fmt::Display for NativeNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from(self.clone()))
    }
}
macro_rules! integer {
    ($name:ident,$min:expr,$max:expr) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "u16", into = "u16")]
        pub struct $name(u16);
        impl TryFrom<u16> for $name {
            type Error = &'static str;
            fn try_from(n: u16) -> Result<Self, Self::Error> {
                if ($min..=$max).contains(&n) {
                    Ok(Self(n))
                } else {
                    Err("invalid_source_number")
                }
            }
        }
        impl From<$name> for u16 {
            fn from(n: $name) -> u16 {
                n.0
            }
        }
        impl $name {
            pub fn get(self) -> u16 {
                self.0
            }
        }
    };
}
integer!(Inches, 1, 100);
integer!(Millimetres, 1, 300);
integer!(BasisPoints, 1, 10000);
integer!(Year, 1000, 9999);
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorSpec {
    pub families: NonEmptySet<ColorFamily>,
    pub maker_shade: Option<NfcText>,
    pub api_v1_display: ColorDisplay,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedSize {
    pub primary: SizedLabel,
    #[serde(default)]
    pub also_reported: Vec<SizedLabel>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizedLabel {
    pub profile: SizingProfileId,
    pub value: SizeValue,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SizeValue {
    Alpha {
        value: AlphaSize,
    },
    AlphaPair {
        first: AlphaSize,
        second: AlphaSize,
    },
    Numeric {
        label: NativeNumber,
    },
    NumericPair {
        first: NativeNumber,
        second: NativeNumber,
    },
    Petite {
        label: NativeNumber,
    },
    Denim {
        waist: Inches,
        inseam: Option<Inches>,
    },
    OneSize,
    Eyewear {
        lens: Millimetres,
        bridge: Millimetres,
        temple: Millimetres,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizingProfile {
    pub system: SizingSystem,
    pub kind: SizeKind,
    pub brand: Option<BrandId>,
    pub scope: Option<SizeScope>,
    pub forms: Option<NonEmptySet<SizeForm>>,
    pub min: Option<NativeNumber>,
    pub max: Option<NativeNumber>,
    #[serde(default = "yes")]
    pub allow_half: bool,
    #[serde(default = "yes")]
    pub allow_double_zero: bool,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Constituent {
    TradeMaterial {
        claim: TradeMaterialClaim,
    },
    Fiber {
        fiber: Fiber,
        #[serde(default)]
        qualifiers: Qualifiers,
    },
    Leather {
        animal: Option<LeatherAnimal>,
        finish: Option<Finish>,
    },
    Rubber,
    Metal {
        metal: Option<Metal>,
    },
    AnimalFill {
        fill: AnimalFill,
    },
    Polymer {
        polymer: Polymer,
        #[serde(default)]
        recycled: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<T>",
    into = "Vec<T>",
    bound(
        serialize = "T: Serialize + Clone",
        deserialize = "T: Deserialize<'de> + Clone"
    )
)]
pub struct NonEmpty<T: Clone>(Vec<T>);
impl<T: Clone> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = &'static str;
    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err("empty_source_list")
        } else {
            Ok(Self(value))
        }
    }
}
impl<T: Clone> From<NonEmpty<T>> for Vec<T> {
    fn from(value: NonEmpty<T>) -> Self {
        value.0
    }
}
impl<T: Clone> std::ops::Deref for NonEmpty<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Clone> std::ops::DerefMut for NonEmpty<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Composition {
    Percent {
        parts: NonEmpty<PercentPart>,
        complete: bool,
    },
    Listed {
        parts: NonEmpty<Constituent>,
        #[serde(default)]
        listed_as_blend: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PercentPart {
    pub constituent: Constituent,
    pub basis_points: BasisPoints,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct MoneyAmount(f64);
impl TryFrom<f64> for MoneyAmount {
    type Error = &'static str;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err("invalid_amount")
        }
    }
}
impl From<MoneyAmount> for f64 {
    fn from(value: MoneyAmount) -> Self {
        value.0
    }
}
impl MoneyAmount {
    pub fn get(self) -> f64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct WeightAmount(f64);
impl TryFrom<f64> for WeightAmount {
    type Error = &'static str;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && value > 0.0 && value <= 10000.0 {
            Ok(Self(value))
        } else {
            Err("invalid_weight")
        }
    }
}
impl From<WeightAmount> for f64 {
    fn from(value: WeightAmount) -> Self {
        value.0
    }
}
impl WeightAmount {
    pub fn get(self) -> f64 {
        self.0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricWeight {
    pub amount: WeightAmount,
    pub unit: WeightUnit,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialComponent {
    pub id: Option<ComponentId>,
    pub color: Option<ColorSpec>,
    pub part: MaterialPart,
    pub construction: Option<Construction>,
    pub composition: Option<Composition>,
    #[serde(default)]
    pub properties: Vec<MaterialProperty>,
    pub weight: Option<FabricWeight>,
    pub maker_wording: Option<PrivateNote>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageReferenceMetadata {
    pub kind: ImageKind,
    pub confidence: ImageConfidence,
    pub source_url: Option<HttpsUrl>,
    pub notes: Option<PrivateNote>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceImage {
    Asset {
        sha256: ImageSha,
        reference: ImageReferenceMetadata,
        crop: Option<ImageCrop>,
    },
    Remote {
        url: HttpsUrl,
        reference: ImageReferenceMetadata,
        crop: Option<ImageCrop>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PurchaseSource {
    Fitting { fitting: FittingId },
    SelfBought,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePrice {
    pub amount: MoneyAmount,
    pub currency: Currency,
    pub basis: NfcText,
    pub quantity_basis: NfcText,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Style {
    pub id: StyleId,
    pub brand: BrandId,
    pub name: NfcText,
    pub category: CategoryId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: VariantId,
    pub style: StyleId,
    pub maker_variant_code: Option<MakerVariantCode>,
    pub color: ColorSpec,
    pub materials: NonEmpty<MaterialComponent>,
    #[serde(default)]
    pub images: Vec<SourceImage>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub id: InventoryId,
    pub variant: VariantId,
    pub source: PurchaseSource,
    pub size: ReportedSize,
    pub quantity: NonZeroU32,
    #[serde(default, skip_serializing_if = "Usage::is_everyday")]
    pub usage: Usage,
    pub sku: Option<MakerSku>,
    pub price: Option<SourcePrice>,
    pub display_name: Option<NfcText>,
    #[serde(default)]
    pub notes: Vec<PrivateNote>,
    pub images: Option<Vec<SourceImage>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brand {
    pub name: NfcText,
    #[serde(default)]
    pub aliases: Vec<NfcText>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub label: NfcText,
    pub parent: Option<CategoryId>,
    pub sizing_kind: Option<SizeKind>,
    #[serde(default)]
    pub aliases: Vec<NfcText>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fitting {
    pub label: NfcText,
    pub season: Option<ShopSeason>,
    pub years: Vec<Year>,
    pub legacy_api_purchase_source: Option<NfcText>,
    pub stylist: Option<NfcText>,
    pub deck_url: Option<HttpsUrl>,
    pub fitting_date: Option<chrono::NaiveDate>,
}
