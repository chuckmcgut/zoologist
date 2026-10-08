use std::path::PathBuf;

use super::*;

const LABELS: &[&str] = &[
    "u1;mammalia;carnivora;canidae;vulpes;vulpes;red fox",
    "u2;mammalia;carnivora;canidae;urocyon;cinereoargenteus;gray fox",
    "u3;mammalia;carnivora;canidae;canis;latrans;coyote",
    "u4;;;;;;blank",
    "u5;mammalia;primates;hominidae;homo;sapiens;human",
    "u6;mammalia;carnivora;canidae;vulpes;lagopus;arctic fox",
    "u7;;;;;;vehicle",
];

const TAXONOMY: &[&str] = &[
    "t1;mammalia;carnivora;canidae;vulpes;;vulpes species",
    "t2;mammalia;carnivora;canidae;;;canine family",
    "t3;mammalia;carnivora;;;;carnivorous mammal",
    "t4;mammalia;;;;;mammal",
];

fn rules(geofence: &str, country: Option<&str>, admin1: Option<&str>) -> SpeciesRules {
    SpeciesRules {
        labels: LABELS
            .iter()
            .map(|l| SpeciesLabel::parse(l).unwrap())
            .collect(),
        taxonomy: Taxonomy::from_lines(TAXONOMY.iter().copied()),
        geofence: Geofence::from_json(geofence).unwrap(),
        country: country.map(str::to_string),
        admin1: admin1.map(str::to_string),
        min_score: 0.65,
        assume: HashMap::new(),
        narrow_to_local: true,
    }
}

/// `rules` with `species.assume` set to `pairs`.
fn rules_assuming(
    geofence: &str,
    country: Option<&str>,
    admin1: Option<&str>,
    pairs: &[(&str, &str)],
) -> SpeciesRules {
    let mut r = rules(geofence, country, admin1);
    let assume = pairs
        .iter()
        .map(|(g, s)| (g.to_string(), s.to_string()))
        .collect();
    r.assume = resolve_assume(&assume, &r.labels, &r.taxonomy).unwrap();
    r
}

/// Probabilities for the six animal/blank/human test labels ("vehicle" gets 0).
fn probs(p: [f32; 6]) -> Vec<f32> {
    let mut v = p.to_vec();
    v.push(0.0);
    v
}

/// Probabilities including "vehicle" (the last label).
fn probs_with_vehicle(p: [f32; 7]) -> Vec<f32> {
    p.to_vec()
}

#[test]
fn label_names() {
    let fox = SpeciesLabel::parse(LABELS[0]).unwrap();
    assert_eq!(fox.full_class(), "mammalia;carnivora;canidae;vulpes;vulpes");
    assert_eq!(fox.scientific_name(), "Vulpes vulpes");
    assert_eq!(fox.display_name(), "Red fox");
    let family = SpeciesLabel::parse(TAXONOMY[1]).unwrap();
    assert_eq!(family.scientific_name(), "Canidae");
    assert!(SpeciesLabel::parse(LABELS[3]).unwrap().is_non_animal());
    assert!(SpeciesLabel::parse(LABELS[4]).unwrap().is_non_animal());
    assert!(!fox.is_non_animal());
    assert!(SpeciesLabel::parse("too;few;parts").is_none());
}

#[test]
fn confident_species_is_named() {
    let r = rules("{}", None, None);
    let g = r
        .decide(&probs([0.9, 0.05, 0.02, 0.01, 0.01, 0.01]), 0.9)
        .unwrap();
    assert_eq!(
        (g.common_name.as_str(), g.scientific_name.as_str()),
        ("Red fox", "Vulpes vulpes")
    );
    assert_eq!(g.candidates.len(), 5);
    // 0.7 is enough when the detector agrees it is an animal...
    let g = r
        .decide(&probs([0.7, 0.1, 0.1, 0.05, 0.0, 0.05]), 0.5)
        .unwrap();
    assert_eq!(g.common_name, "Red fox");
}

#[test]
fn unsure_results_roll_up_to_genus_then_family() {
    let r = rules("{}", None, None);
    // ...but not when the detector is unsure: Vulpes genus = red + arctic fox = 0.75.
    let g = r
        .decide(&probs([0.7, 0.1, 0.1, 0.05, 0.0, 0.05]), 0.1)
        .unwrap();
    assert_eq!(g.scientific_name, "Vulpes");
    // Split between genera: the family Canidae is sure (0.4 + 0.35 = 0.75).
    let g = r
        .decide(&probs([0.4, 0.35, 0.0, 0.25, 0.0, 0.0]), 0.9)
        .unwrap();
    assert_eq!(
        (g.scientific_name.as_str(), g.common_name.as_str()),
        ("Canidae", "Canine family")
    );
    // Nothing reaches 0.65 at any level below "animal": unidentified.
    assert!(
        r.decide(&probs([0.3, 0.1, 0.1, 0.5, 0.0, 0.0]), 0.9)
            .is_none()
    );
}

#[test]
fn non_animal_top_labels_give_no_species() {
    let r = rules("{}", None, None);
    assert!(
        r.decide(&probs([0.02, 0.0, 0.0, 0.97, 0.01, 0.0]), 0.9)
            .is_none()
    );
    assert!(
        r.decide(&probs([0.02, 0.0, 0.0, 0.0, 0.98, 0.0]), 0.9)
            .is_none()
    );
}

#[test]
fn confident_non_animals_are_relabelled() {
    let r = rules("{}", None, None);
    // Labels: [0] deer, [1] roe deer, [2] fox, [3] blank, [4] human, [5] vehicle.
    assert_eq!(
        r.not_an_animal(&probs([0.02, 0.0, 0.0, 0.0, 0.98, 0.0])),
        Some(Label::Person)
    );
    // A person on a quad bike: human and vehicle split the probability.
    assert_eq!(
        r.not_an_animal(&probs_with_vehicle([
            0.04, 0.0, 0.02, 0.04, 0.22, 0.0, 0.68
        ])),
        Some(Label::Vehicle)
    );
    assert_eq!(
        r.not_an_animal(&probs_with_vehicle([
            0.04, 0.0, 0.02, 0.04, 0.68, 0.0, 0.22
        ])),
        Some(Label::Person)
    );
    // Glare or moving leaves: "blank" alone.
    assert_eq!(
        r.not_an_animal(&probs([0.01, 0.0, 0.0, 0.97, 0.01, 0.01])),
        Some(Label::Motion)
    );
    // A real animal, or anything unclear, stays an animal.
    assert_eq!(
        r.not_an_animal(&probs([0.9, 0.05, 0.02, 0.01, 0.01, 0.01])),
        None
    );
    assert_eq!(
        r.not_an_animal(&probs([0.14, 0.0, 0.1, 0.61, 0.05, 0.1])),
        None
    );
    assert_eq!(
        r.not_an_animal(&probs([0.2, 0.0, 0.0, 0.1, 0.7, 0.0])),
        None
    );
}

#[test]
fn geofence_rules() {
    let json = r#"{
        "mammalia;carnivora;canidae;vulpes;vulpes": {"allow": {"USA": [], "CAN": []}},
        "mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}},
        "mammalia;carnivora;canidae;canis;latrans": {"block": {"GBR": [], "MEX": ["YUC"]}}
    }"#;
    let g = Geofence::from_json(json).unwrap();
    let label = |i: usize| SpeciesLabel::parse(LABELS[i]).unwrap();
    assert!(!g.blocks(&label(0), Some("USA"), Some("NY")));
    assert!(g.blocks(&label(0), Some("DEU"), None)); // country not in allow list
    assert!(!g.blocks(&label(0), None, None)); // no location: never blocked
    assert!(g.blocks(&label(5), Some("USA"), Some("NY"))); // arctic fox only in Alaska
    assert!(!g.blocks(&label(5), Some("USA"), Some("AK")));
    assert!(g.blocks(&label(2), Some("GBR"), Some("ENG"))); // whole country blocked
    assert!(g.blocks(&label(2), Some("MEX"), Some("YUC")));
    assert!(!g.blocks(&label(2), Some("MEX"), Some("SON")));
    assert!(!g.blocks(&label(1), Some("GBR"), None)); // no rule: allowed
}

#[test]
fn geofenced_species_rolls_up_instead() {
    let json = r#"{"mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}}}"#;
    let r = rules(json, Some("USA"), Some("NY"));
    // "Arctic fox" 0.85 in New York is not possible there, and the red fox is the only fox of
    // its genus that is: it is a red fox.
    let g = r
        .decide(&probs([0.1, 0.0, 0.0, 0.05, 0.0, 0.85]), 0.9)
        .unwrap();
    assert_eq!(g.common_name, "Red fox");
    assert_eq!(g.candidates[0].0, "Arctic fox");

    // With no fox of that genus left either, the answer rolls up. Like SpeciesNet, it starts at
    // family level (genus is skipped).
    let json = r#"{
        "mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}},
        "mammalia;carnivora;canidae;vulpes;vulpes": {"block": {"USA": ["NY"]}}
    }"#;
    let r = rules(json, Some("USA"), Some("NY"));
    let g = r
        .decide(&probs([0.1, 0.0, 0.0, 0.05, 0.0, 0.85]), 0.9)
        .unwrap();
    assert_eq!(g.scientific_name, "Canidae");
    assert!(g.score > 0.85);
}

#[test]
fn a_group_with_one_local_species_is_that_species() {
    let json = r#"{"mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}}}"#;
    // Split between red and arctic fox: the genus Vulpes is sure (0.75).
    let split = probs([0.4, 0.0, 0.0, 0.25, 0.0, 0.35]);
    // In New York only the red fox lives, so it is one.
    let g = rules(json, Some("USA"), Some("NY"))
        .decide(&split, 0.9)
        .unwrap();
    assert_eq!(
        (g.scientific_name.as_str(), g.common_name.as_str()),
        ("Vulpes vulpes", "Red fox")
    );
    assert!((g.score - 0.75).abs() < 1e-5);
    // In Alaska both live, and without a location nothing is known: the genus stays.
    for r in [
        rules(json, Some("USA"), Some("AK")),
        rules(json, None, None),
    ] {
        assert_eq!(r.decide(&split, 0.9).unwrap().scientific_name, "Vulpes");
    }
    // So it does with `narrow_to_local` off.
    let mut off = rules(json, Some("USA"), Some("NY"));
    off.narrow_to_local = false;
    assert_eq!(off.decide(&split, 0.9).unwrap().scientific_name, "Vulpes");
    // Only a genus is narrowed. A family stays, even with one local species among those the
    // classifier knows: it does not know them all.
    let json = r#"{
        "mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}},
        "mammalia;carnivora;canidae;urocyon;cinereoargenteus": {"block": {"USA": ["NY"]}},
        "mammalia;carnivora;canidae;canis;latrans": {"block": {"USA": ["NY"]}}
    }"#;
    let g = rules(json, Some("USA"), Some("NY"))
        .decide(&probs([0.4, 0.0, 0.35, 0.25, 0.0, 0.0]), 0.9)
        .unwrap();
    assert_eq!(g.scientific_name, "Canidae");
}

#[test]
fn an_assumed_species_replaces_its_group() {
    let r = rules_assuming("{}", None, None, &[("Canidae", "Coyote")]);
    // The family, and a genus in it, become the assumed species...
    let g = r
        .decide(&probs([0.4, 0.35, 0.0, 0.25, 0.0, 0.0]), 0.9)
        .unwrap();
    assert_eq!(
        (g.scientific_name.as_str(), g.common_name.as_str()),
        ("Canis latrans", "Coyote")
    );
    let g = r
        .decide(&probs([0.4, 0.0, 0.0, 0.25, 0.0, 0.35]), 0.9)
        .unwrap();
    assert_eq!(g.common_name, "Coyote");
    // ...but a species the classifier is sure of keeps its name, and a wider group is not touched.
    let g = r
        .decide(&probs([0.9, 0.05, 0.02, 0.01, 0.01, 0.01]), 0.9)
        .unwrap();
    assert_eq!(g.common_name, "Red fox");
    let r = rules_assuming("{}", None, None, &[("vulpes", "vulpes vulpes")]);
    let g = r
        .decide(&probs([0.4, 0.35, 0.0, 0.25, 0.0, 0.0]), 0.9)
        .unwrap();
    assert_eq!(g.scientific_name, "Canidae");

    // The nearest group wins, and an assumed species beats the only local one.
    let json = r#"{"mammalia;carnivora;canidae;vulpes;lagopus": {"allow": {"USA": ["AK"]}}}"#;
    let r = rules_assuming(
        json,
        Some("USA"),
        Some("NY"),
        &[("canidae", "coyote"), ("vulpes", "arctic fox")],
    );
    let g = r
        .decide(&probs([0.4, 0.0, 0.0, 0.25, 0.0, 0.35]), 0.9)
        .unwrap();
    assert_eq!(g.common_name, "Arctic fox");
}

#[test]
fn assume_must_name_known_groups_and_species() {
    let r = rules("{}", None, None);
    let check = |group: &str, species: &str| {
        let assume = [(group.to_string(), species.to_string())].into();
        resolve_assume(&assume, &r.labels, &r.taxonomy)
    };
    assert!(check("canidae", "red fox").is_ok());
    assert!(check("canine family", "Vulpes vulpes").is_ok());
    assert!(check("felidae", "red fox").unwrap_err().contains("felidae"));
    assert!(check("canidae", "wolf").unwrap_err().contains("wolf"));
    // A group is not a species, and the species must be in the group.
    assert!(check("canidae", "vulpes species").is_err());
    assert!(check("vulpes", "coyote").unwrap_err().contains("belong"));
}

#[test]
fn voting_weights_crops() {
    let v = vote(&[(vec![1.0, 0.0], 3.0), (vec![0.0, 1.0], 1.0)]);
    assert!((v[0] - 0.75).abs() < 1e-6 && (v[1] - 0.25).abs() < 1e-6);
    assert!(vote(&[]).is_empty());
    let s = softmax(&[1.0, 2.0, 3.0]);
    assert!((s.iter().sum::<f32>() - 1.0).abs() < 1e-6 && s[2] > s[1]);
}

// ---- Golden test with the real model (skipped when models/ is not installed) ----

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The deer of a station in New York, with the real labels, taxonomy and geofence: there the
/// geofence leaves white-tailed deer and moose.
#[test]
fn deer_in_new_york() {
    let models = root().join("models");
    if !models.join("speciesnet_geofence.json").is_file() {
        eprintln!("speciesnet not installed (scripts/fetch-models.sh); skipping");
        return;
    }
    let read = |name: &str| std::fs::read_to_string(models.join(name)).unwrap();
    let labels: Vec<SpeciesLabel> = read("speciesnet_labels.txt")
        .lines()
        .filter_map(SpeciesLabel::parse)
        .collect();
    let taxonomy = Taxonomy::from_lines(read("speciesnet_taxonomy.txt").lines());
    let assume = [("cervidae".to_string(), "white-tailed deer".to_string())].into();
    let assume = resolve_assume(&assume, &labels, &taxonomy).unwrap();
    let mut r = SpeciesRules {
        labels,
        taxonomy,
        geofence: Geofence::from_json(&read("speciesnet_geofence.json")).unwrap(),
        country: Some("USA".into()),
        admin1: Some("NY".into()),
        min_score: 0.65,
        assume: HashMap::new(),
        narrow_to_local: true,
    };
    let name = |r: &SpeciesRules, scores: &[(&str, f32)]| {
        let mut probs = vec![0.0; r.labels.len()];
        for (common, p) in scores {
            probs[r.labels.iter().position(|l| l.common == *common).unwrap()] = *p;
        }
        r.decide(&probs, 0.9).map(|g| g.common_name)
    };
    let two_deer = [("white-tailed deer", 0.45), ("mule deer", 0.35)];
    let mule = [("mule deer", 0.9)];
    let family = [("white-tailed deer", 0.3), ("moose", 0.25), ("elk", 0.25)];
    let elk = [("elk", 0.9)];
    let moose = [("moose", 0.9)];

    // The genus Odocoileus has one species in New York.
    let white_tail = Some("White-tailed deer".to_string());
    assert_eq!(name(&r, &two_deer), white_tail);
    assert_eq!(name(&r, &mule), white_tail);
    // The family has two, and no elk: those stay "deer family" until `assume` says otherwise.
    assert_eq!(name(&r, &family), Some("Cervidae family".to_string()));
    assert_eq!(name(&r, &elk), Some("Cervidae family".to_string()));
    r.assume = assume;
    assert_eq!(name(&r, &family), white_tail);
    assert_eq!(name(&r, &elk), white_tail);
    // A moose is still a moose.
    assert_eq!(name(&r, &moose), Some("Moose".to_string()));
}

#[test]
fn golden_speciesnet_on_test_photos() {
    let models = root().join("models");
    if !models.join("speciesnet.onnx").is_file() {
        eprintln!("speciesnet not installed (scripts/fetch-models.sh); skipping");
        return;
    }
    let cfg = SpeciesConfig {
        path: models.join("speciesnet.onnx"),
        labels: models.join("speciesnet_labels.txt"),
        taxonomy: Some(models.join("speciesnet_taxonomy.txt")),
        geofence: Some(models.join("speciesnet_geofence.json")),
        ..SpeciesConfig::default()
    };
    let station = StationConfig {
        country: Some("USA".into()),
        admin1_region: Some("NY".into()),
        ..StationConfig::default()
    };
    let model = SpeciesModel::load(&cfg, &station).unwrap();
    let golden: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root().join("tools/fixtures/golden/speciesnet.json")).unwrap(),
    )
    .unwrap();
    for (photo, expected) in golden.as_object().unwrap() {
        let img = image::open(root().join("tools/fixtures/images").join(photo))
            .unwrap()
            .to_rgb8();
        let (w, h) = (img.width() as f32, img.height() as f32);
        let bb: Vec<f32> = expected["bbox"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let (mx, my) = ((bb[2] - bb[0]) * 0.1, (bb[3] - bb[1]) * 0.1);
        let x1 = ((bb[0] - mx).max(0.0) * w) as u32;
        let y1 = ((bb[1] - my).max(0.0) * h) as u32;
        let x2 = ((bb[2] + mx).min(1.0) * w) as u32;
        let y2 = ((bb[3] + my).min(1.0) * h) as u32;
        let crop = image::imageops::crop_imm(&img, x1, y1, x2 - x1, y2 - y1).to_image();
        let input =
            crate::detector::resize_rgb(crop.as_raw(), crop.width(), crop.height(), INPUT_SIZE)
                .unwrap();
        let p = model.classify(&input).unwrap();
        let (best, score) = p
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let want = &expected["top5"][0];
        assert_eq!(
            best as u64,
            want["index"].as_u64().unwrap(),
            "{photo}: top label differs"
        );
        let want_score = want["score"].as_f64().unwrap() as f32;
        assert!(
            (score - want_score).abs() < 0.05,
            "{photo}: {score} vs {want_score}"
        );

        let guess = model.rules.decide(&p, 0.9);
        println!("{photo}: {guess:?}");
        let name = guess.map(|g| g.common_name).unwrap_or_default();
        let expected_name = want["label"].as_str().unwrap();
        // Ringtails do not live in New York; everything else should be named as is.
        if expected_name == "ringtail" {
            assert_ne!(
                name.to_lowercase(),
                "ringtail",
                "{photo}: geofence should apply"
            );
        } else if want_score > 0.8 {
            assert_eq!(name.to_lowercase(), expected_name, "{photo}");
        }
    }
}

/// Always votes for whichever class the crop is brightest in (red → 0, else 1), after a delay.
struct FakeClassifier;

impl SpeciesClassifier for FakeClassifier {
    fn classify(&self, rgb: &[u8]) -> Result<Vec<f32>, DetectorError> {
        assert_eq!(rgb.len(), (INPUT_SIZE * INPUT_SIZE * 3) as usize);
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(if rgb[0] > 150 {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        })
    }
    fn decide(&self, probs: &[f32], _: f32) -> Option<SpeciesGuess> {
        (probs[0] > 0.5).then(|| SpeciesGuess {
            scientific_name: "Rubrum".into(),
            common_name: "Red thing".into(),
            score: probs[0],
            model_id: "fake".into(),
            candidates: vec![],
        })
    }
}

fn crop(rgb: [u8; 3], quality: f32) -> SpeciesCrop {
    let pixels: Vec<u8> = (0..64 * 64).flat_map(|_| rgb).collect();
    SpeciesCrop {
        frame: Frame {
            camera_id: "cam".into(),
            seq: 0,
            captured_at: chrono::Utc::now(),
            width: 64,
            height: 64,
            i420: Arc::new(zoologist_core::yuv::rgb_to_i420(&pixels, 64, 64).unwrap()),
        },
        bbox: BBox::new(0.2, 0.2, 0.8, 0.8),
        quality,
    }
}

#[test]
fn pool_votes_across_crops_and_drops_when_full() {
    let (handle, _threads) = spawn_species_pool(Arc::new(FakeClassifier), 1, 2).unwrap();
    // Two red crops outweigh one blue crop.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle.submit(SpeciesJob {
        check: Default::default(),
        crops: vec![
            crop([230, 20, 20], 1.0),
            crop([20, 20, 230], 1.0),
            crop([230, 20, 20], 1.0),
        ],
        detector_score: 0.9,
        reply: tx,
    });
    let SpeciesAnswer::Species(guess) = rx.blocking_recv().unwrap() else {
        panic!("expected a species");
    };
    assert!((guess.score - 2.0 / 3.0).abs() < 1e-3, "{}", guess.score);

    // Overload: a queue of 2 cannot hold 6 quick submissions of slow jobs.
    let replies: Vec<_> = (0..6)
        .map(|_| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle.submit(SpeciesJob {
                check: Default::default(),
                crops: vec![crop([230, 20, 20], 1.0)],
                detector_score: 0.9,
                reply: tx,
            });
            rx
        })
        .collect();
    let answered: Vec<_> = replies
        .into_iter()
        .map(|rx| rx.blocking_recv().unwrap())
        .collect();
    assert!(handle.dropped() > 0);
    assert!(
        answered
            .iter()
            .any(|a| matches!(a, SpeciesAnswer::Species(_)))
    );
    assert!(
        answered.contains(&SpeciesAnswer::Unknown),
        "dropped jobs answer Unknown"
    );
    assert_eq!(handle.queue_depth(), 0);
}

#[test]
fn a_still_animal_that_cannot_be_named_is_motion() {
    let named = SpeciesAnswer::Species(SpeciesGuess {
        scientific_name: "aves".into(),
        common_name: "Bird".into(),
        score: 0.8,
        model_id: "speciesnet".into(),
        candidates: Vec::new(),
    });
    // Never moved, no confident answer: a stump or a shadow.
    assert_eq!(
        settle_still_animal(SpeciesAnswer::Unknown, false, true),
        SpeciesAnswer::NotAnimal(Label::Motion)
    );
    // It moved: kept as an unnamed animal.
    assert_eq!(
        settle_still_animal(SpeciesAnswer::Unknown, true, true),
        SpeciesAnswer::Unknown
    );
    // Named: kept, moved or not (a bird sitting on a branch).
    assert_eq!(settle_still_animal(named.clone(), false, true), named);
    // The rule turned off.
    assert_eq!(
        settle_still_animal(SpeciesAnswer::Unknown, false, false),
        SpeciesAnswer::Unknown
    );
}

#[test]
fn only_nothing_there_changes_a_person() {
    assert_eq!(
        settle_person(SpeciesAnswer::NotAnimal(Label::Motion)),
        Some(Label::Motion),
        "an insect in the infrared light"
    );
    assert_eq!(settle_person(SpeciesAnswer::NotAnimal(Label::Person)), None);
    assert_eq!(
        settle_person(SpeciesAnswer::NotAnimal(Label::Vehicle)),
        None
    );
    assert_eq!(settle_person(SpeciesAnswer::Unknown), None);
}
