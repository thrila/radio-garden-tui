use std::io::Read;
use std::time::Duration;

fn main() {
    eprintln!("[test] Building HTTP client...");
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let redirect_client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // 1. Get random place
    eprintln!("[test] Fetching places...");
    let places_raw: serde_json::Value = client
        .get("https://radio.garden/api/ara/content/places")
        .send()
        .unwrap()
        .json()
        .unwrap();

    let places = places_raw["data"]["list"].as_array().unwrap();
    let place = &places[rand_range(0, places.len())];
    let place_id = place["id"].as_str().unwrap();
    let place_title = place["title"].as_str().unwrap();
    let place_country = place["country"].as_str().unwrap();
    eprintln!("[test] Selected place: {place_title}, {place_country} ({place_id})");

    // 2. Get channels for that place
    eprintln!("[test] Fetching channels...");
    let channels_raw: serde_json::Value = client
        .get(format!(
            "https://radio.garden/api/ara/content/page/{place_id}/channels"
        ))
        .send()
        .unwrap()
        .json()
        .unwrap();

    let mut channel_list = Vec::new();
    if let Some(content) = channels_raw["data"]["content"].as_array() {
        for section in content {
            if let Some(items) = section["items"].as_array() {
                for item in items {
                    let page = &item["page"];
                    if let (Some(title), Some(href)) =
                        (page["title"].as_str(), page["url"].as_str())
                    {
                        channel_list.push((title.to_string(), href.to_string()));
                    }
                }
            }
        }
    }

    if channel_list.is_empty() {
        eprintln!("[test] No channels found for {place_title}, trying another place...");
        return;
    }

    let (channel_title, channel_href) = &channel_list[rand_range(0, channel_list.len())];
    let channel_id = channel_href.split('/').next_back().unwrap();
    eprintln!("[test] Selected station: {channel_title} ({channel_id})");

    // 3. Resolve stream URL via HEAD redirects
    let listen_url =
        format!("https://radio.garden/api/ara/content/listen/{channel_id}/channel.mp3");
    eprintln!("[test] Resolving stream: {listen_url}");

    let mut current = listen_url.clone();
    for i in 0..5 {
        match redirect_client.head(&current).send() {
            Ok(resp) => {
                eprintln!("[test]   Hop {i}: {} {current}", resp.status());
                if resp.status().is_redirection()
                    && let Some(loc) = resp.headers().get("location")
                {
                    let loc_str = loc.to_str().unwrap_or("");
                    current = if loc_str.starts_with("http") {
                        loc_str.to_string()
                    } else {
                        let base = reqwest::Url::parse(&current).unwrap();
                        base.join(loc_str).unwrap().to_string()
                    };
                    continue;
                }
                break;
            }
            Err(e) => {
                eprintln!("[test]   Hop {i}: ERROR {e}");
                break;
            }
        }
    }
    eprintln!("[test] Final stream URL: {current}");

    // 4. GET the stream
    eprintln!("[test] Streaming...");
    let mut resp = match client.get(&current).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[test] GET failed: {e}");
            return;
        }
    };
    eprintln!("[test] Status: {}", resp.status());
    eprintln!(
        "[test] Content-Type: {:?}",
        resp.headers().get("content-type")
    );

    // 5. Buffer initial data
    let mut data = Vec::with_capacity(256 * 1024);
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    while total < 256 * 1024 {
        match resp.read(&mut buf) {
            Ok(0) => {
                eprintln!("[test] Stream ended at {total} bytes");
                break;
            }
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                total += n;
            }
            Err(e) => {
                eprintln!("[test] Read error at {total} bytes: {e}");
                break;
            }
        }
    }
    eprintln!("[test] Buffered {total} bytes");

    // 6. Decode and play
    eprintln!("[test] Decoding...");
    let cursor = std::io::Cursor::new(data);
    match rodio::Decoder::new(cursor) {
        Ok(dec) => {
            eprintln!("[test] Decoder OK - PLAYING: {channel_title} on {place_title}");
            let sink = rodio::DeviceSinkBuilder::open_default_sink().unwrap();
            let mixer = sink.mixer().clone();
            let player = rodio::Player::connect_new(&mixer);
            player.append(dec);
            player.set_volume(0.5);
            eprintln!("[test] Ctrl+C to stop");
            player.sleep_until_end();
            eprintln!("[test] Done");
        }
        Err(e) => {
            eprintln!("[test] Decoder FAILED: {e}");
        }
    }
}

fn rand_range(min: usize, max: usize) -> usize {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    );
    (h.finish() as usize) % (max - min) + min
}
