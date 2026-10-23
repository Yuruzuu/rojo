use blake3::Hasher;
use rbx_dom_weak::types::{ContentType, PhysicalProperties, Variant, VariantType, Vector3};

macro_rules! n_hash {
    ($hash:ident, $($num:expr),*) => {
        {$(
            $hash.update(&($num).to_le_bytes());
        )*}
    };
}

macro_rules! hash {
    ($hash:ident, $value:expr) => {{
        $hash.update($value);
    }};
}

/// Places `value` into the provided hasher.
pub fn hash_variant(hasher: &mut Hasher, value: &Variant) {
    // Equal hashes skip serialization, so numeric edits must keep their full precision.
    n_hash!(hasher, value.ty() as u32);
    match value {
        Variant::Attributes(attrs) => {
            let mut sorted: Vec<(&String, &Variant)> = attrs.iter().collect();
            sorted.sort_unstable_by_key(|(name, _)| *name);
            for (name, attribute) in sorted {
                hasher.update(name.as_bytes());
                match attribute {
                    // UTF-8 binary attributes serialize as ordinary strings in metadata.
                    Variant::BinaryString(bytes) if std::str::from_utf8(bytes.as_ref()).is_ok() => {
                        n_hash!(hasher, VariantType::String as u32);
                        hasher.update(bytes.as_ref());
                    }
                    _ => hash_variant(hasher, attribute),
                }
            }
        }
        Variant::Axes(a) => hash!(hasher, &[a.bits()]),
        Variant::BinaryString(bytes) => hash!(hasher, bytes.as_ref()),
        Variant::Bool(bool) => hash!(hasher, &[*bool as u8]),
        Variant::BrickColor(color) => n_hash!(hasher, *color as u16),
        Variant::CFrame(cf) => {
            vector_hash(hasher, cf.position);
            vector_hash(hasher, cf.orientation.x);
            vector_hash(hasher, cf.orientation.y);
            vector_hash(hasher, cf.orientation.z);
        }
        Variant::Color3(color) => n_hash!(hasher, color.r, color.g, color.b),
        Variant::Color3uint8(color) => hash!(hasher, &[color.r, color.b, color.g]),
        Variant::ColorSequence(seq) => {
            let mut new = Vec::with_capacity(seq.keypoints.len());
            for keypoint in &seq.keypoints {
                new.push(keypoint);
            }
            new.sort_unstable_by(|a, b| a.time.total_cmp(&b.time));
            for keypoint in new {
                n_hash!(
                    hasher,
                    keypoint.time,
                    keypoint.color.r,
                    keypoint.color.g,
                    keypoint.color.b
                )
            }
        }
        Variant::Content(content) => match content.value() {
            ContentType::None => {
                hash!(hasher, &[0]);
            }
            ContentType::Uri(uri) => {
                hash!(hasher, &[1]);
                hash!(hasher, uri.as_bytes());
            }
            ContentType::Object(referent) => {
                hash!(hasher, &[2]);
                hash!(hasher, referent.to_string().as_bytes())
            }
            other => {
                panic!("the ContentType {other:?} cannot be hashed as a Variant")
            }
        },
        Variant::ContentId(content) => {
            let s: &str = content.as_ref();
            hash!(hasher, s.as_bytes())
        }
        Variant::Enum(e) => n_hash!(hasher, e.to_u32()),
        Variant::EnumItem(e) => {
            hash!(hasher, e.ty.as_bytes());
            n_hash!(hasher, e.value);
        }
        Variant::Faces(f) => hash!(hasher, &[f.bits()]),
        Variant::Float32(n) => n_hash!(hasher, n),
        Variant::Float64(n) => n_hash!(hasher, n),
        Variant::Font(f) => {
            n_hash!(hasher, f.weight as u16);
            n_hash!(hasher, f.style as u8);
            hash!(hasher, f.family.as_bytes());
            if let Some(cache) = &f.cached_face_id {
                hash!(hasher, &[0x01]);
                hash!(hasher, cache.as_bytes());
            } else {
                hash!(hasher, &[0x00]);
            }
        }
        Variant::Int32(n) => n_hash!(hasher, n),
        Variant::Int64(n) => n_hash!(hasher, n),
        Variant::MaterialColors(n) => hash!(hasher, n.encode().as_slice()),
        Variant::NetAssetRef(net_asset) => hash!(hasher, net_asset.hash().as_bytes()),
        Variant::NumberRange(nr) => n_hash!(hasher, nr.max, nr.min),
        Variant::NumberSequence(seq) => {
            let mut new = Vec::with_capacity(seq.keypoints.len());
            for keypoint in &seq.keypoints {
                new.push(keypoint);
            }
            new.sort_unstable_by(|a, b| a.time.total_cmp(&b.time));
            for keypoint in new {
                n_hash!(hasher, keypoint.time, keypoint.value, keypoint.envelope)
            }
        }
        Variant::OptionalCFrame(maybe_cf) => {
            if let Some(cf) = maybe_cf {
                hash!(hasher, &[0x01]);
                vector_hash(hasher, cf.position);
                vector_hash(hasher, cf.orientation.x);
                vector_hash(hasher, cf.orientation.y);
                vector_hash(hasher, cf.orientation.z);
            } else {
                hash!(hasher, &[0x00]);
            }
        }
        Variant::PhysicalProperties(properties) => match properties {
            PhysicalProperties::Default => hash!(hasher, &[0x00]),
            PhysicalProperties::Custom(custom) => {
                hash!(hasher, &[0x00]);
                n_hash!(
                    hasher,
                    custom.density(),
                    custom.friction(),
                    custom.elasticity(),
                    custom.friction_weight(),
                    custom.elasticity_weight(),
                    custom.acoustic_absorption()
                )
            }
        },
        Variant::Ray(ray) => {
            vector_hash(hasher, ray.origin);
            vector_hash(hasher, ray.direction);
        }
        Variant::Rect(rect) => n_hash!(hasher, rect.max.x, rect.max.y, rect.min.x, rect.min.y),
        Variant::Ref(referent) => hash!(hasher, referent.to_string().as_bytes()),
        Variant::Region3(region) => {
            vector_hash(hasher, region.max);
            vector_hash(hasher, region.min);
        }
        Variant::Region3int16(region) => {
            n_hash!(
                hasher,
                region.max.x,
                region.max.y,
                region.max.z,
                region.min.x,
                region.min.y,
                region.min.z
            )
        }
        Variant::SecurityCapabilities(capabilities) => n_hash!(hasher, capabilities.bits()),
        Variant::SharedString(sstr) => hash!(hasher, sstr.hash().as_bytes()),
        Variant::String(str) => hash!(hasher, str.as_bytes()),
        Variant::Tags(tags) => {
            let mut dupe: Vec<&str> = tags.iter().collect();
            dupe.sort_unstable();
            for tag in dupe {
                hash!(hasher, tag.as_bytes())
            }
        }
        Variant::UDim(udim) => n_hash!(hasher, udim.scale, udim.offset),
        Variant::UDim2(udim) => n_hash!(
            hasher,
            udim.y.scale,
            udim.y.offset,
            udim.x.scale,
            udim.x.offset
        ),
        Variant::Vector2(v2) => n_hash!(hasher, v2.x, v2.y),
        Variant::Vector2int16(v2) => n_hash!(hasher, v2.x, v2.y),
        Variant::Vector3(v3) => vector_hash(hasher, *v3),
        Variant::Vector3int16(v3) => n_hash!(hasher, v3.x, v3.y, v3.z),

        // Hashing UniqueId properties doesn't make sense
        Variant::UniqueId(_) => (),

        unknown => {
            log::warn!(
                "Encountered unknown Variant {:?} while hashing",
                unknown.ty()
            )
        }
    }
}

fn vector_hash(hasher: &mut Hasher, vector: Vector3) {
    n_hash!(hasher, vector.x, vector.y, vector.z)
}

#[cfg(test)]
mod test {
    use super::*;
    use rbx_dom_weak::types::{
        Attributes, Color3, NumberSequence, NumberSequenceKeypoint, Vector2,
    };

    fn digest(value: &Variant) -> blake3::Hash {
        let mut hasher = Hasher::new();
        hash_variant(&mut hasher, value);
        hasher.finalize()
    }

    #[test]
    fn close_float_values_have_distinct_hashes() {
        assert_ne!(
            digest(&Variant::Float32(0.11)),
            digest(&Variant::Float32(0.14))
        );
        assert_ne!(
            digest(&Variant::Float64(0.11)),
            digest(&Variant::Float64(0.14))
        );
    }

    #[test]
    fn different_variant_types_cannot_alias_the_same_payload() {
        assert_ne!(
            digest(&Variant::Float64(0.25)),
            digest(&Variant::Vector2(Vector2::new(0.0, 1.625))),
        );
    }

    #[test]
    fn utf8_binary_attributes_hash_like_their_metadata_representation() {
        let attributes = |value| {
            let mut attributes = Attributes::new();
            attributes.insert("Rojo_Id".into(), value);
            Variant::Attributes(attributes)
        };

        assert_eq!(
            digest(&attributes(Variant::String("stable".into()))),
            digest(&attributes(Variant::BinaryString(
                b"stable".to_vec().into()
            ))),
        );
        assert_ne!(
            digest(&Variant::String("stable".into())),
            digest(&Variant::BinaryString(b"stable".to_vec().into())),
        );
    }

    #[test]
    fn small_vector_and_color_edits_have_distinct_hashes() {
        assert_ne!(
            digest(&Variant::Vector3(Vector3::new(0.11, 1.0, 2.0))),
            digest(&Variant::Vector3(Vector3::new(0.14, 1.0, 2.0)))
        );
        assert_ne!(
            digest(&Variant::Color3(Color3::new(0.11, 0.5, 1.0))),
            digest(&Variant::Color3(Color3::new(0.14, 0.5, 1.0)))
        );
    }

    #[test]
    fn numeric_attribute_edits_have_distinct_hashes() {
        let attributes = |value| {
            let mut attributes = Attributes::new();
            attributes.insert("Speed".into(), Variant::Float64(value));
            Variant::Attributes(attributes)
        };

        assert_ne!(digest(&attributes(0.11)), digest(&attributes(0.14)));
    }

    #[test]
    fn sequences_with_nan_times_hash_deterministically() {
        let sequence = Variant::NumberSequence(NumberSequence {
            keypoints: vec![
                NumberSequenceKeypoint::new(0.0, 0.0, 0.0),
                NumberSequenceKeypoint::new(f32::NAN, 0.5, 0.0),
                NumberSequenceKeypoint::new(1.0, 1.0, 0.0),
            ],
        });

        assert_eq!(digest(&sequence), digest(&sequence));
    }
}
