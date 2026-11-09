//! Deterministic displays and checks. No quantities, colors or fiber ratios are inferred.
use crate::source::{Diagnostics, types::*};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn color(spec: &ColorSpec, path: &str, errors: &mut Diagnostics) -> Option<String> {
    match spec.api_v1_display {
        ColorDisplay::Families => Some(
            spec.families
                .iter()
                .map(|family| family.label())
                .collect::<Vec<_>>()
                .join(" / "),
        ),
        ColorDisplay::MakerShade => match &spec.maker_shade {
            Some(shade) => Some(shade.to_string()),
            None => {
                errors.add(format!("{path}.maker_shade"), "maker_shade_required");
                None
            }
        },
    }
}
fn leather_animal_label(animal: LeatherAnimal) -> String {
    match animal {
        LeatherAnimal::Bovine
        | LeatherAnimal::Buffalo
        | LeatherAnimal::Crocodile
        | LeatherAnimal::Alligator
        | LeatherAnimal::Ostrich
        | LeatherAnimal::Lizard => format!("{} leather", animal.label()),
        LeatherAnimal::Calf
        | LeatherAnimal::Lamb
        | LeatherAnimal::Sheep
        | LeatherAnimal::Goat
        | LeatherAnimal::Pig
        | LeatherAnimal::Deer
        | LeatherAnimal::Horse
        | LeatherAnimal::Snake => animal.label().into(),
    }
}

fn constituent_label(part: &Constituent) -> String {
    match part {
        Constituent::Fiber { fiber, qualifiers } => {
            let mut words = qualifiers.iter().map(|q| q.label()).collect::<Vec<_>>();
            words.push(fiber.label());
            words.join(" ")
        }
        Constituent::Leather { animal, finish } => match (animal, finish) {
            (Some(animal), Some(finish)) => {
                format!("{} {}", finish.label(), leather_animal_label(*animal))
            }
            (Some(animal), None) => leather_animal_label(*animal),
            (None, None) => "Leather".into(),
            (None, Some(finish)) => match finish {
                Finish::Suede | Finish::WaxedSuede | Finish::Nubuck | Finish::Shearling => {
                    finish.label().into()
                }
                Finish::Smooth
                | Finish::Patent
                | Finish::Grained
                | Finish::Pebbled
                | Finish::Embossed
                | Finish::Nappa
                | Finish::Box
                | Finish::Saffiano
                | Finish::HairOn => format!("{} leather", finish.label()),
            },
        },
        Constituent::Rubber => "Rubber".into(),
        Constituent::Metal { metal } => metal.map_or("Metal", Metal::label).into(),
        Constituent::AnimalFill { fill } => fill.label().into(),
        Constituent::Polymer { polymer, recycled } => {
            if *recycled {
                format!("Recycled {}", polymer.label())
            } else {
                polymer.label().into()
            }
        }
        Constituent::TradeMaterial { claim } => claim.label().into(),
    }
}
fn validate_constituent(part: &Constituent, path: &str, errors: &mut Diagnostics) {
    if let Constituent::Fiber { fiber, qualifiers } = part {
        use Fiber::*;
        use FiberQualifier::*;
        let natural = matches!(
            fiber,
            Cotton
                | Linen
                | Silk
                | Wool
                | Cashmere
                | Mohair
                | CamelHair
                | Alpaca
                | Angora
                | Hemp
                | Raffia
                | Sisal
        );
        let animal = matches!(
            fiber,
            Wool | Cashmere | Mohair | CamelHair | Alpaca | Angora
        );
        for qualifier in qualifiers.iter() {
            let valid = match qualifier {
                Organic => natural,
                Recycled => true,
                Virgin => animal,
                Merino | Lambswool => *fiber == Wool,
                Pima | Regenerative => *fiber == Cotton,
            };
            errors.check(
                valid,
                format!("{path}.qualifiers"),
                "incompatible_fiber_qualifier",
            );
        }
        errors.check(
            !(qualifiers.contains(Recycled) && qualifiers.contains(Virgin)),
            format!("{path}.qualifiers"),
            "incompatible_fiber_qualifiers",
        );
    }
}
pub(crate) fn normalize_materials(components: &mut [MaterialComponent]) {
    components.sort_by(|a, b| a.part.cmp(&b.part).then_with(|| a.id.cmp(&b.id)));
    for component in components {
        component.properties.sort();
        match &mut component.composition {
            Some(Composition::Listed { parts, .. }) => parts.sort(),
            Some(Composition::Percent { parts, .. }) => {
                parts.sort_by(|a, b| a.constituent.cmp(&b.constituent))
            }
            None => {}
        }
    }
}
pub(crate) fn materials(
    components: &[MaterialComponent],
    path: &str,
    errors: &mut Diagnostics,
) -> Option<String> {
    errors.check(components.len() <= 64, path, "too_many_material_components");
    let mut part_counts = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for component in components {
        *part_counts.entry(component.part).or_insert(0) += 1;
    }
    let mut displays = Vec::new();
    for (index, component) in components.iter().enumerate() {
        let path = format!("{path}[{index}]");
        errors.check(
            component.composition.is_some() || component.construction.is_some(),
            &path,
            "material_facts_required",
        );
        if part_counts[&component.part] > 1 {
            errors.check(
                component.id.is_some(),
                format!("{path}.id"),
                "component_identity_required",
            );
        }
        if let Some(id) = &component.id {
            errors.check(
                ids.insert(id),
                format!("{path}.id"),
                "duplicate_component_id",
            );
        }
        let properties: BTreeSet<_> = component.properties.iter().collect();
        errors.check(
            properties.len() == component.properties.len(),
            format!("{path}.properties"),
            "duplicate_material_property",
        );
        let mut display = match &component.composition {
            Some(Composition::Listed {
                parts,
                listed_as_blend,
            }) => {
                errors.check(
                    !parts.is_empty() && parts.len() <= 64,
                    format!("{path}.composition.parts"),
                    "invalid_composition_parts",
                );
                let mut seen = BTreeSet::new();
                for (i, part) in parts.iter().enumerate() {
                    validate_constituent(part, &format!("{path}.composition.parts[{i}]"), errors);
                    errors.check(
                        seen.insert(part),
                        format!("{path}.composition.parts[{i}]"),
                        "duplicate_constituent",
                    );
                }
                let mut label = parts
                    .iter()
                    .map(constituent_label)
                    .collect::<Vec<_>>()
                    .join(", ");
                if *listed_as_blend {
                    label.push_str(" blend");
                }
                label
            }
            Some(Composition::Percent { parts, complete }) => {
                errors.check(
                    !parts.is_empty() && parts.len() <= 64,
                    format!("{path}.composition.parts"),
                    "invalid_composition_parts",
                );
                let total: u32 = parts
                    .iter()
                    .map(|part| u32::from(part.basis_points.get()))
                    .sum();
                errors.check(
                    if *complete {
                        total == 10000
                    } else {
                        total < 10000
                    },
                    format!("{path}.composition"),
                    "invalid_composition_total",
                );
                let mut seen = BTreeSet::new();
                for (i, part) in parts.iter().enumerate() {
                    validate_constituent(
                        &part.constituent,
                        &format!("{path}.composition.parts[{i}].constituent"),
                        errors,
                    );
                    errors.check(
                        seen.insert(&part.constituent),
                        format!("{path}.composition.parts[{i}]"),
                        "duplicate_constituent",
                    );
                }
                parts
                    .iter()
                    .map(|part| {
                        let label = constituent_label(&part.constituent);
                        if *complete && parts.len() == 1 && part.basis_points.get() == 10000 {
                            label
                        } else {
                            let percent = f64::from(part.basis_points.get()) / 100.0;
                            format!("{percent}% {label}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            None => String::new(),
        };
        if let Some(construction) = component.construction {
            if display.is_empty() {
                display = construction.label().into();
            } else {
                display.push_str(&format!(" ({})", construction.label()));
            }
        }
        if let Some(weight) = &component.weight {
            display.push_str(&format!(
                " ({} {})",
                weight.amount.get(),
                weight.unit.label()
            ));
        }
        if !component.properties.is_empty() {
            display = format!(
                "{} {display}",
                component
                    .properties
                    .iter()
                    .map(|property| property.label())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if let Some(spec) = &component.color
            && let Some(color) = color(spec, &format!("{path}.color"), errors)
        {
            display = format!("{color} {display}");
        }
        if component.part != MaterialPart::Main {
            display = format!("{}: {display}", component.part.label());
        }
        displays.push(display);
    }
    if displays.is_empty() {
        None
    } else {
        let value = displays.join("; ");
        errors.check(value.len() <= 1024, path, "projection_text_too_long");
        Some(value)
    }
}
fn expected_kind(
    category: &CategoryId,
    categories: &BTreeMap<CategoryId, Category>,
) -> Option<SizeKind> {
    let mut current = Some(category);
    for _ in 0..=categories.len() {
        let id = current?;
        let definition = categories.get(id)?;
        if let Some(kind) = definition.sizing_kind {
            return Some(kind);
        }
        current = definition.parent.as_ref();
    }
    None
}
pub(crate) fn inferred_kind(category: &CategoryId) -> Option<SizeKind> {
    let c = category.as_str();
    if c == "clothing.jeans" || c.starts_with("clothing.jeans.") {
        Some(SizeKind::Denim)
    } else if c == "clothing" || c.starts_with("clothing.") {
        Some(SizeKind::Clothing)
    } else if c == "footwear" || c.starts_with("footwear.") {
        Some(SizeKind::Footwear)
    } else if c == "belts" || c.starts_with("belts.") {
        Some(SizeKind::Belt)
    } else if c == "sunglasses" || c.starts_with("sunglasses.") {
        Some(SizeKind::Eyewear)
    } else if c == "bags" || c.starts_with("bags.") || c == "hats" || c.starts_with("hats.") {
        Some(SizeKind::Accessory)
    } else {
        None
    }
}
fn form(value: &SizeValue) -> SizeForm {
    match value {
        SizeValue::Alpha { .. } => SizeForm::Alpha,
        SizeValue::AlphaPair { .. } => SizeForm::AlphaPair,
        SizeValue::Numeric { .. } => SizeForm::Numeric,
        SizeValue::Petite { .. } => SizeForm::Petite,
        SizeValue::Denim { .. } => SizeForm::Denim,
        SizeValue::OneSize => SizeForm::OneSize,
        SizeValue::Eyewear { .. } => SizeForm::Eyewear,
    }
}
fn default_form(kind: SizeKind, form: SizeForm) -> bool {
    match kind {
        SizeKind::Clothing => matches!(
            form,
            SizeForm::Alpha
                | SizeForm::AlphaPair
                | SizeForm::Numeric
                | SizeForm::Petite
                | SizeForm::OneSize
        ),
        SizeKind::Footwear => matches!(
            form,
            SizeForm::Alpha | SizeForm::AlphaPair | SizeForm::Numeric | SizeForm::OneSize
        ),
        SizeKind::Denim => matches!(
            form,
            SizeForm::Alpha
                | SizeForm::AlphaPair
                | SizeForm::Numeric
                | SizeForm::Denim
                | SizeForm::OneSize
        ),
        SizeKind::Belt => matches!(
            form,
            SizeForm::Alpha | SizeForm::AlphaPair | SizeForm::Numeric | SizeForm::OneSize
        ),
        SizeKind::Eyewear => matches!(
            form,
            SizeForm::Alpha | SizeForm::AlphaPair | SizeForm::Eyewear | SizeForm::OneSize
        ),
        SizeKind::Accessory => matches!(
            form,
            SizeForm::Alpha | SizeForm::AlphaPair | SizeForm::Numeric | SizeForm::OneSize
        ),
    }
}
pub(crate) fn validate_profile(profile: &SizingProfile, path: &str, errors: &mut Diagnostics) {
    errors.check(
        profile.kind == SizeKind::Footwear
            || [&profile.min, &profile.max]
                .into_iter()
                .flatten()
                .all(|number| {
                    !matches!(
                        number,
                        NativeNumber::OneThird(_) | NativeNumber::TwoThirds(_)
                    )
                }),
        path,
        "size_thirds_not_allowed",
    );
    let ceiling = if profile.kind == SizeKind::Belt {
        500
    } else {
        100
    };
    errors.check(
        profile
            .min
            .as_ref()
            .is_none_or(|n| n.sixths() <= ceiling * 6)
            && profile
                .max
                .as_ref()
                .is_none_or(|n| n.sixths() <= ceiling * 6),
        path,
        "invalid_profile_bounds",
    );
    if let (Some(min), Some(max)) = (&profile.min, &profile.max) {
        errors.check(min.sixths() <= max.sixths(), path, "invalid_profile_bounds");
    }
    if let Some(forms) = &profile.forms {
        for form in forms.iter() {
            errors.check(
                default_form(profile.kind, *form),
                format!("{path}.forms"),
                "incompatible_size_form",
            );
        }
    }
}
pub(crate) fn size(
    size: &ReportedSize,
    brand: &BrandId,
    category: &CategoryId,
    profiles: &BTreeMap<SizingProfileId, SizingProfile>,
    categories: &BTreeMap<CategoryId, Category>,
    path: &str,
    errors: &mut Diagnostics,
) -> String {
    errors.check(
        size.also_reported.len() <= 8,
        format!("{path}.also_reported"),
        "too_many_reported_sizes",
    );
    let mut seen = BTreeSet::new();
    let mut labels = Vec::new();
    for (index, label) in std::iter::once(&size.primary)
        .chain(&size.also_reported)
        .enumerate()
    {
        let path = if index == 0 {
            format!("{path}.primary")
        } else {
            format!("{path}.also_reported[{}]", index - 1)
        };
        let Some(profile) = profiles.get(&label.profile) else {
            // The compiler checks every reference before contextual sizing,
            // even when an unrelated style/brand reference is invalid.
            continue;
        };
        errors.check(
            profile.brand.as_ref().is_none_or(|b| b == brand),
            format!("{path}.profile"),
            "size_profile_brand_mismatch",
        );
        let category_kind = expected_kind(category, categories).or_else(|| inferred_kind(category));
        errors.check(
            category_kind.is_some_and(|kind| {
                kind == profile.kind
                    || kind == SizeKind::Denim && profile.kind == SizeKind::Clothing
            }),
            format!("{path}.profile"),
            "size_profile_category_mismatch",
        );
        let form = form(&label.value);
        let permitted = profile.forms.as_ref().map_or_else(
            || default_form(profile.kind, form),
            |forms| forms.iter().any(|allowed| *allowed == form),
        );
        errors.check(
            permitted,
            format!("{path}.value"),
            "size_profile_form_mismatch",
        );
        let rendered = match &label.value {
            SizeValue::Alpha { value } => value.label().into(),
            SizeValue::AlphaPair { first, second } => {
                errors.check(
                    first != second,
                    format!("{path}.value"),
                    "duplicate_alpha_pair",
                );
                format!("{}/{}", first.label(), second.label())
            }
            SizeValue::Numeric { label } | SizeValue::Petite { label } => {
                let max = profile.max.as_ref().map_or(
                    if profile.kind == SizeKind::Belt {
                        3000
                    } else {
                        600
                    },
                    NativeNumber::sixths,
                );
                let min = profile.min.as_ref().map_or(0, NativeNumber::sixths);
                errors.check(
                    (min..=max).contains(&label.sixths()),
                    format!("{path}.value.label"),
                    "size_out_of_bounds",
                );
                errors.check(
                    profile.allow_half || !matches!(label, NativeNumber::Half(_)),
                    format!("{path}.value.label"),
                    "size_half_not_allowed",
                );
                errors.check(
                    profile.kind == SizeKind::Footwear
                        || !matches!(
                            label,
                            NativeNumber::OneThird(_) | NativeNumber::TwoThirds(_)
                        ),
                    format!("{path}.value.label"),
                    "size_thirds_not_allowed",
                );
                errors.check(
                    !matches!(label, NativeNumber::DoubleZero)
                        || (profile.allow_double_zero && profile.kind == SizeKind::Clothing),
                    format!("{path}.value.label"),
                    "size_double_zero_not_allowed",
                );
                format!("{label}{}", if form == SizeForm::Petite { "P" } else { "" })
            }
            SizeValue::Denim { waist, inseam } => format!(
                "W{}{}",
                waist.get(),
                inseam.map_or_else(String::new, |i| format!(" / L{}", i.get()))
            ),
            SizeValue::OneSize => "OS".into(),
            SizeValue::Eyewear {
                lens,
                bridge,
                temple,
            } => {
                errors.check(
                    lens.get() <= 100 && bridge.get() <= 50 && temple.get() >= 50,
                    format!("{path}.value"),
                    "invalid_eyewear_dimensions",
                );
                format!("{}-{}-{} mm", lens.get(), bridge.get(), temple.get())
            }
        };
        let mut prefixes = Vec::new();
        if let Some(scope) = profile.scope {
            prefixes.push(scope.label());
        }
        if profile.system != SizingSystem::Native {
            prefixes.push(profile.system.label());
        }
        let value = if prefixes.is_empty() {
            rendered
        } else {
            format!("{} {rendered}", prefixes.join(" "))
        };
        errors.check(
            seen.insert((label.profile.clone(), label.value.clone())),
            &path,
            "duplicate_reported_size",
        );
        labels.push(value);
    }
    let rendered = labels.join(" / ");
    errors.check(rendered.len() <= 1024, path, "projection_text_too_long");
    rendered
}
