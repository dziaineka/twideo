extern crate lazy_static;
extern crate redis;

use rand::seq::SliceRandom;
use redis::Commands;
use regex::Regex;
use std::{env, error::Error, fmt};
use twitter_video_dl::serde_schemes::*;

const DISABLED: &str = "disabled";

lazy_static::lazy_static! {
    static ref TWITTER_STATUS_URL: &'static str = "https://api.twitter.com/1.1/statuses/show.json?extended_entities=true&tweet_mode=extended&id=";
    static ref TWITTER_V2_URL: &'static str = "https://api.twitter.com/2/tweets?expansions=author_id&ids=";

    static ref TWITTER_BEARER_TOKENS: Vec<String> = vec![
        env::var("TWITTER_BEARER_TOKEN").unwrap_or_else(|_| "".to_string()),
        env::var("TWITTER_BEARER_TOKEN2").unwrap_or_else(|_| "".to_string())
    ].into_iter().filter(|x| !x.is_empty()).collect::<Vec<String>>();

    static ref TWITTER_MULTIMEDIA_URL: &'static str = "https://api.twitter.com/2/tweets";
    static ref TWITTER_SEARCH_URL: &'static str = "https://api.twitter.com/2/tweets/search/recent";
    static ref TWITTER_EXPANSIONS_PARAMS: &'static str = "expansions=attachments.media_keys,author_id&media.fields=url,variants,preview_image_url&user.fields=name";
    static ref RE : regex::Regex= Regex::new("https://t.co/\\w+\\b").unwrap();
    static ref REDIS_URL: String = env::var("REDIS_URL").unwrap_or_else(|_| "".to_string());
    static ref THREADS_SUPPORT: String = env::var("THREADS_SUPPORT").unwrap_or_else(|_| DISABLED.to_string());
}

pub fn get_twitter_id(link: &str) -> TwitterID {
    if link.contains("twitter.com/i/spaces/") {
        return TwitterID::None;
    }

    let parsed: Vec<&str> = (link[20..]).split('/').collect();
    let last_parts: Vec<&str> = parsed.last().unwrap().split('?').collect();
    let possible_id = last_parts.first().unwrap().parse().unwrap_or(0);

    if possible_id > 0 {
        TwitterID::Id(possible_id)
    } else {
        TwitterID::None
    }
}

#[derive(Debug)]
struct MyError(String);

impl fmt::Display for MyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "There is an error: {}", self.0)
    }
}

impl Error for MyError {}

#[derive(Debug)]
pub struct TwitterMedia {
    pub url: String,
    pub r#type: String,
    pub thumb: String,
}

#[derive(Debug)]
pub struct TwitDetails {
    pub caption: String,
    pub twitter_media: Vec<TwitterMedia>,
    pub name: String,
    pub id: u64,
    pub extra_urls: Vec<Variant>,
    pub conversation_id: u64,
    pub next: u8,
    pub user_id: u64,
    pub thread_count: usize,
}

pub enum TwitterID {
    Id(u64),
    None,
}

pub async fn get_twitter_data(
    twitter_id: u64,
) -> Result<Option<TwitDetails>, Box<dyn std::error::Error>> {
    log::info!("Send request to twitter");

    let token = TWITTER_BEARER_TOKENS
        .choose(&mut rand::thread_rng())
        .unwrap()
        .to_string();

    let client = reqwest::Client::new();

    let multimedia_response = client
        .get(format!(
            "{}/{}?tweet.fields=conversation_id&{}",
            &*TWITTER_MULTIMEDIA_URL, twitter_id, &*TWITTER_EXPANSIONS_PARAMS
        ))
        .header("AUTHORIZATION", format!("Bearer {}", token))
        .send()
        .await?;

    log::info!("Status {}", multimedia_response.status().as_u16());

    if multimedia_response.status().as_u16() == 401 {
        return Err(Box::new(MyError("Unauthorized Error!".into())));
    }

    if multimedia_response.status().as_u16() == 429 {
        return Ok(None);
    }

    let multimedia = multimedia_response.json::<MultimediaBody>().await?;

    let mut twitter_media: Vec<TwitterMedia> = Vec::new();
    let mut extra_urls: Vec<Variant> = Vec::new();
    let mut name = String::new();
    let mut username = String::new();
    let conversation_id = multimedia
        .data
        .conversation_id
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let user_id = multimedia.data.author_id.unwrap().parse::<u64>().unwrap();

    let thread_count = if *THREADS_SUPPORT == DISABLED {
        0
    } else {
        fetch_threads(conversation_id, user_id).await
    };

    if let Some(includes) = &multimedia.includes {
        name = includes.users[0].name.to_string();
        username = includes.users[0].username.to_string();

        if let Some(media_set) = &includes.media {
            for media in media_set {
                if media.r#type == "video" || media.r#type == "animated_gif" {
                    let mut last_bitrate = 0;
                    let mut last_url = "";
                    let mut alternative_url = "";

                    for variant in media.variants.as_ref().unwrap() {
                        if let Some(bitrate) = variant.bit_rate {
                            extra_urls.push(variant.clone());
                            if bitrate >= last_bitrate {
                                last_url = variant.url.as_str();
                                last_bitrate = bitrate;
                            }
                        } else {
                            alternative_url = variant.url.as_str();
                        }
                    }

                    if !last_url.is_empty() {
                        twitter_media.push(TwitterMedia {
                            url: last_url.to_string(),
                            r#type: media.r#type.to_string(),
                            thumb: media.preview_image_url.as_ref().unwrap().to_owned(),
                        });
                    } else if !alternative_url.is_empty() {
                        twitter_media.push(TwitterMedia {
                            url: alternative_url.to_string(),
                            r#type: media.r#type.to_string(),
                            thumb: media.preview_image_url.as_ref().unwrap().to_owned(),
                        });
                    }
                } else if media.r#type == "photo" {
                    let _url = media.url.as_ref().unwrap().to_string();
                    twitter_media.push(TwitterMedia {
                        url: _url.to_string(),
                        r#type: media.r#type.to_string(),
                        thumb: _url,
                    });
                }
            }
        }
    }

    let mut clean_caption = None;
    let tweet_text = multimedia.data.text.as_ref().unwrap();

    let captures: Vec<&str> = RE
        .captures_iter(tweet_text)
        .map(|c| c.get(0).unwrap().as_str())
        .collect();

    if !captures.is_empty() {
        let mut captured = captures[captures.len() - 1];

        // means tweet doesn's contain media, so the link is real link (not media link)
        if twitter_media.is_empty() {
            clean_caption = Some(tweet_text.replace(captured, &format!("\n{}", captured)));
        } else {
            clean_caption = Some(tweet_text.replace(captured, "")); // remove media link
            if captures.len() > 1 {
                captured = captures[captures.len() - 2];
                clean_caption = Some(
                    clean_caption
                        .as_ref()
                        .unwrap()
                        .replace(captured, &format!("\n{}", captured)),
                );
            }
        }
    }

    Ok(Some(TwitDetails {
        caption: format!(
            "{} \n\n<a href='https://twitter.com/{}/status/{}'>&#x1F464 {}</a>",
            || -> &str {
                if clean_caption.is_none() {
                    return tweet_text;
                }
                return clean_caption.as_ref().unwrap();
            }(),
            username,
            twitter_id,
            name
        ),
        twitter_media,
        name,
        id: twitter_id,
        extra_urls,
        next: 1,
        conversation_id,
        thread_count,
        user_id,
    }))
}

const CONVERSATION_KEY: &str = "conversation";
const EXPIRE_KEY_TTL: u32 = 24 * 60 * 60;

async fn fetch_threads(conversation_id: u64, user_id: u64) -> usize {
    // check cache if fetch threads before
    let client = redis::Client::open(&**REDIS_URL);

    if client.is_err() {
        return 0;
    }

    let mut con = client.unwrap().get_connection().unwrap();
    let redis_key = format!("{}:{}", CONVERSATION_KEY, conversation_id);

    let mut threads_count: usize = con.hlen(redis_key.clone()).unwrap_or(0);

    if threads_count > 0 {
        log::info!("threads exists in cache");
        return threads_count;
    }

    log::info!("fetch thread");

    let token = TWITTER_BEARER_TOKENS
        .choose(&mut rand::thread_rng())
        .unwrap()
        .to_string();

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "{0}?query=conversation_id:{1} from:{2} to:{2}&tweet.fields=author_id,referenced_tweets&max_results=100",
            &*TWITTER_SEARCH_URL, conversation_id, user_id
        ))
        .header("AUTHORIZATION", format!("Bearer {}", token))
        .send()
        .await;

    if response.is_err() {
        log::info!("fetch thread failed");
        return 0;
    }

    let result = response.unwrap();
    log::info!("Status {}", result.status().as_u16());

    let response_json = result.json::<ThreadSearchResult>().await.unwrap();
    let mut search_data = response_json.data.unwrap_or_default();

    let mut thread_ids: Vec<u64> = vec![];
    let mut last_reference: u64 = 0;

    while !search_data.is_empty() {
        let obj = search_data.pop().unwrap();
        let current_id = obj.id.parse::<u64>().unwrap();

        if last_reference == 0 {
            // first thread
            last_reference = current_id;
            thread_ids.push(current_id);

            continue;
        }

        let reference = obj
            .referenced_tweets
            .into_iter()
            .find(|x| x.r#type == "replied_to");

        if let Some(reference) = reference {
            let reference_id = reference.id.parse::<u64>().unwrap();
            if reference_id == last_reference {
                last_reference = current_id;
                thread_ids.push(current_id);
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // save thread_ids to cache
    threads_count = thread_ids.len();

    if threads_count == 0 {
        return 0;
    }

    let mut pipe = redis::pipe();

    for (i, id) in thread_ids.iter().enumerate() {
        pipe.cmd("HSET").arg(redis_key.clone()).arg(i + 1).arg(id);
    }
    pipe.cmd("EXPIRE")
        .arg(redis_key.clone())
        .arg(EXPIRE_KEY_TTL);

    let _: () = pipe.query(&mut con).unwrap();

    threads_count
}

pub async fn get_thread(conversation_id: u64, thread_number: u8, user_id: u64) -> Option<u64> {
    let client = redis::Client::open(&**REDIS_URL);

    if client.is_err() {
        return None;
    }

    let mut con = client.unwrap().get_connection().unwrap();
    let redis_key = format!("{}:{}", CONVERSATION_KEY, conversation_id);

    let mut tid: String = con
        .hget(redis_key.clone(), thread_number)
        .unwrap_or_else(|_| "".to_string());

    if !tid.is_empty() {
        return Some(tid.parse::<u64>().unwrap());
    }

    let thread_count = fetch_threads(conversation_id, user_id).await;

    if thread_count > 0 {
        tid = con.hget(redis_key, thread_number).unwrap();
        return Some(tid.parse::<u64>().unwrap());
    }

    None
}
