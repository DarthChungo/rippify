use colored::Colorize;
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{
    authentication::Credentials, config::SessionConfig, session::Session, spotify_id::SpotifyId,
    Error, FileId,
};

use getopts::{Fail, Options};
use librespot_metadata::{audio::AudioFileFormat, Album, Artist, Metadata, Playlist, Track};
use oggvorbismeta::{replace_comment_header, CommentHeader, VorbisComments};
use regex::Regex;
use std::{
    collections::{HashSet, VecDeque},
    env,
    io::{Cursor, Read},
    path::Path,
    process::exit,
};
use tokio::{
    fs::{create_dir_all, File},
    io::copy,
};

#[tokio::main]
async fn main() {
    let opts = match parse_opts() {
        Ok(opts) => opts,
        Err(err) => {
            println!("{}: {}", "error".red().bold(), err.to_string().bold());
            exit(1);
        }
    };

    let credentials = Credentials::with_password(&opts.user, &opts.pass);
    let session_config = SessionConfig::default();

    let session = Session::new(session_config, None);

    match session.connect(credentials, false).await {
        Ok(_) => {
            println!(
                "{} Logged in as: {}",
                "=>".green().bold(),
                &opts.user.bright_blue()
            );
        }
        Err(err) => {
            println!(
                "{}: cannot log in: {}",
                "error".red().bold(),
                err.to_string().to_lowercase()
            );
            exit(1);
        }
    };

    let mut track_ids: HashSet<SpotifyId> = HashSet::new();

    println!("\n{} Input resources:", "=>".green().bold());

    for line in &opts.input {
        if let Some((id, id_str)) = get_resource_from_line(line, "track") {
            println!(" {} track: {}", "->".yellow().bold(), &id_str);
            track_ids.insert(id);
            //
        } else if let Some((id, id_str)) = get_resource_from_line(line, "playlist") {
            println!(" {} playlist: {}", "->".yellow().bold(), &id_str);

            if let Err(err) = get_playlist_from_id(&session, &id, &mut track_ids).await {
                println!(
                    "{}: cannot get playlist metadata: {}, skipping...",
                    "warning".yellow().bold(),
                    err
                );
                continue;
            }
        //
        } else if let Some((id, id_str)) = get_resource_from_line(line, "album") {
            println!(" {} album: {}", "->".yellow().bold(), &id_str);

            if let Err(err) = get_album_from_id(&session, &id, &mut track_ids).await {
                println!(
                    "{}: cannot get album metadata: {}, skipping...",
                    "warning".yellow().bold(),
                    err
                );
            }
        //
        } else if let Some((id, id_str)) = get_resource_from_line(line, "artist") {
            println!(" {} artist: {}", "->".yellow().bold(), &id_str);

            if let Err(err) = get_artist_from_id(&session, &id, &mut track_ids).await {
                println!(
                    "{}: cannot get artist metadata: {}, skipping...",
                    "warning".yellow().bold(),
                    err
                );
            }
        } else {
            println!(
                "{}: unrecognized input: {}, skipping...",
                " -> warning".yellow().bold(),
                line.bold()
            );
        }
    }

    if track_ids.is_empty() {
        println!(
            "\n{}: didn't get any tracks, aborting...",
            "error".red().bold()
        );
        exit(0);
    }

    println!(
        "\n{} Parsed {} tracks:",
        "=>".green().bold(),
        track_ids.len().to_string().bold()
    );

    let mut tracks_completed: usize = 0;
    let mut tracks_existing: usize = 0;

    for track_id in &track_ids {
        print!(" {} ", "->".yellow().bold());

        let (track, track_file_id) = match get_track_from_id(&session, track_id).await {
            Ok((track, file_id)) => {
                if track.id.to_base62().unwrap() != track_id.to_base62().unwrap() {
                    println!(
                        "{} ({} alt. {})",
                        track.name.bold(),
                        track.id.to_base62().unwrap(),
                        track_id.to_base62().unwrap()
                    );
                } else {
                    println!("{} ({})", track.name.bold(), track.id.to_base62().unwrap());
                }

                (track, file_id)
            }
            Err(e) => {
                println!("{} ({})", "??".bold(), track_id.to_base62().unwrap());
                println!(
                    "   - {}: cannot get track from id: {}, skipping...",
                    "warning".yellow().bold(),
                    e,
                );
                continue;
            }
        };

        let track_output_path = opts
            .format
            .clone()
            .replace("{author}", &track.artists.first().unwrap().name) // NOTE: using the first found artist as the "main" artist
            .replace("{album}", &track.album.name)
            .replace("{name}", &track.name.as_str().replace('/', " "))
            .replace("{ext}", "ogg");

        if Path::new(&track_output_path).exists() {
            println!(
                "   - {}: output file \"{}\" already exists, skipping...",
                "note".bright_blue().bold(),
                track_output_path
            );
            tracks_existing += 1;
            continue;
        }

        let slice_pos = match track_output_path.rfind('/') {
            Some(pos) => pos,
            None => {
                println!(
                    "{}: invalid format string {}, aborting...",
                    "error".red().bold(),
                    opts.format.bold()
                );
                exit(1);
            }
        };

        let track_folder_path = &track_output_path[..slice_pos + 1];

        if create_dir_all(track_folder_path).await.is_err() {
            print!(
                "   - {}: cannot create folders: {}, aborting...",
                "warning".yellow().bold(),
                track_folder_path
            );
            exit(1);
        }

        let track_file_key = match session.audio_key().request(track.id, track_file_id).await {
            Ok(key) => key,
            Err(err) => {
                println!(
                    "   - {}: cannot get audio key: {:?}, skipping",
                    "warning".yellow().bold(),
                    err
                );
                continue;
            }
        };

        let mut track_buffer = Vec::<u8>::new();
        let mut track_buffer_decrypted = Vec::<u8>::new();

        println!("   - getting encrypted audio file");

        let mut track_file_audio = match AudioFile::open(&session, track_file_id, 40).await {
            Ok(audio) => audio,
            Err(err) => {
                println!(
                    "   - {}: cannot get audio file: {:?}, skipping",
                    "warning".yellow().bold(),
                    err
                );
                continue;
            }
        };

        match track_file_audio.read_to_end(&mut track_buffer) {
            Ok(_) => {}
            Err(err) => {
                println!(
                    "   - {}: cannot get track file audio: {}, skipping",
                    "warning".yellow().bold(),
                    err
                );
                continue;
            }
        };

        println!("   - decrypting audio");

        match AudioDecrypt::new(Some(track_file_key), &track_buffer[..])
            .read_to_end(&mut track_buffer_decrypted)
        {
            Ok(_) => {}
            Err(err) => {
                println!(
                    "   - {}: cannot decrypt audio file: {}, skipping",
                    "warning".yellow().bold(),
                    err
                );
                continue;
            }
        };

        println!("   - writing output file");

        let track_file_cursor = Cursor::new(&track_buffer_decrypted[0xa7..]);
        let mut track_comments = CommentHeader::new();

        track_comments.set_vendor("Ogg");

        track_comments.add_tag_single("title", &track.name);
        track_comments.add_tag_single("album", &track.album.name);

        track
            .artists
            .iter()
            .for_each(|artist| track_comments.add_tag_single("artist", &artist.name));

        let mut track_file_out = replace_comment_header(track_file_cursor, track_comments);

        let mut track_file_write = File::create(&track_output_path).await.unwrap();
        match copy(&mut track_file_out, &mut track_file_write).await {
            Ok(_) => {
                println!("   - wrote \"{}\"", track_output_path);
            }
            Err(err) => {
                println!(
                    "   - {}: cannot write {}: {}, skipping...",
                    "warning".yellow().bold(),
                    track_output_path,
                    err
                );
                continue;
            }
        };

        tracks_completed += 1;
    }

    println!("\n{} Processed tracks: ", "=>".green().bold(),);

    println!(
        " {} {} error",
        "->".yellow().bold(),
        track_ids.len() - tracks_completed - tracks_existing
    );

    println!(
        " {} {} already downloaded",
        "->".yellow().bold(),
        tracks_existing
    );

    println!(" {} {} new", "->".yellow().bold(), tracks_completed);

    println!(
        " {} {} total processed",
        "->".yellow().bold(),
        track_ids.len()
    )
}

struct UserParams {
    user: String,
    pass: String,
    format: String,
    input: Vec<String>,
}

fn parse_opts() -> Result<UserParams, Fail> {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = Options::new();

    opts.optflag("h", "help", "print the help menu");

    opts.optopt("u", "user", "user login name, required", "USER");
    opts.optopt("p", "pass", "user password, required", "PASS");
    opts.optopt(
        "f",
        "format",
        "output format to use. {author}/{album}/{name}.{ext} is used by default. Available format specifiers are: {author}, {album}, {name} and {ext}. Note that when tracks have more that one author, {author} will evaluate only to main one (track metadata will still we written correctly).",
        "FMT",
    );

    let matches = opts.parse(&args[1..])?;
    let input = matches.free.clone();

    if matches.opt_present("h")
        || !matches.opt_present("u")
        || !matches.opt_present("p")
        || input.is_empty()
    {
        print_usage(&program, opts);
        exit(0);
    }

    let format = if let Some(format) = matches.opt_str("f") {
        format
    } else {
        "{author}/{album}/{name}.{ext}".to_owned()
    };

    let user = matches.opt_str("u").unwrap();
    let pass = matches.opt_str("p").unwrap();

    Ok(UserParams {
        user,
        pass,
        format,
        input,
    })
}

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} [OPTIONS] URIs...", program);
    print!("{}", opts.usage(&brief));
}

async fn get_track_from_id(session: &Session, id: &SpotifyId) -> Result<(Track, FileId), Error> {
    let mut track_ids = VecDeque::<SpotifyId>::new();
    track_ids.push_back(id.to_owned());

    while let Some(id) = track_ids.pop_front() {
        let track = match Track::get(session, &id).await {
            Ok(track) => track,
            Err(e) => return Err(e),
        };

        match track
            .files
            .get_key_value(&AudioFileFormat::OGG_VORBIS_320)
            .or(track.files.get_key_value(&AudioFileFormat::OGG_VORBIS_160))
            .or(track.files.get_key_value(&AudioFileFormat::OGG_VORBIS_96))
        {
            Some(format) => return Ok((track.to_owned(), format.1.to_owned())),
            None => track_ids.extend(track.alternatives.0),
        };
    }

    Err(Error::internal("cannot find a suitable track"))
}

async fn get_playlist_from_id(
    session: &Session,
    id: &SpotifyId,
    existing_tracks: &mut HashSet<SpotifyId>,
) -> Result<(), Error> {
    let playlist = match Playlist::get(&session, &id).await {
        Ok(playlist) => playlist,
        Err(err) => return Err(err),
    };

    for track in playlist.tracks() {
        existing_tracks.insert(track.to_owned());
    }

    Ok(())
}

async fn get_album_from_id(
    session: &Session,
    id: &SpotifyId,
    existing_tracks: &mut HashSet<SpotifyId>,
) -> Result<(), Error> {
    let album = match Album::get(&session, &id).await {
        Ok(album) => album,
        Err(err) => return Err(err),
    };

    for track in album.tracks() {
        existing_tracks.insert(track.to_owned());
    }

    Ok(())
}

async fn get_artist_from_id(
    session: &Session,
    id: &SpotifyId,
    existing_tracks: &mut HashSet<SpotifyId>,
) -> Result<(), Error> {
    let artist = match Artist::get(&session, &id).await {
        Ok(album) => album,
        Err(err) => return Err(err),
    };

    for album_group in artist.albums.0 {
        for album in album_group.0 .0 {
            get_album_from_id(session, &album, existing_tracks).await?;
        }
    }

    for album_group in artist.singles.0 {
        for album in album_group.0 .0 {
            get_album_from_id(session, &album, existing_tracks).await?;
        }
    }

    Ok(())
}

fn get_resource_from_line<'a>(line: &'a str, name: &str) -> Option<(SpotifyId, &'a str)> {
    let resource_uri = Regex::new(&format!(r"^spotify:{}:([[:alnum:]]{{22}})$", name)).unwrap();
    let resource_url = Regex::new(&format!(
        r"^(http(s)?://)?open\.spotify\.com/{}/([[:alnum:]]{{22}})$",
        name
    ))
    .unwrap();

    if let Some(captures) = resource_uri.captures(line).or(resource_url.captures(line)) {
        let id_str = captures.iter().last().unwrap().unwrap().as_str();
        let id = SpotifyId::from_base62(id_str).unwrap();

        Some((id, id_str))
    //
    } else {
        None
    }
}
