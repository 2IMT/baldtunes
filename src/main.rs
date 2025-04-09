use audiotags::Tag;
use rand::{rng, seq::SliceRandom};
use serenity::{
    all::{
        CacheHttp, ChannelType, Command, CommandInteraction, CommandOptionType, Context,
        CreateCommand, CreateCommandOption, CreateInteractionResponse,
        CreateInteractionResponseMessage, EventHandler, GatewayIntents, GuildId, Interaction,
    },
    async_trait,
    model::gateway::Ready,
    Client,
};
use songbird::{
    input::{
        codecs::{get_codec_registry, get_probe},
        File, Input,
    },
    tracks::Track,
    Call, SerenityInit,
};
use std::{
    cmp::min,
    collections::{hash_map::Entry, HashMap},
    env,
    error::Error,
    sync::Arc,
};
use tokio::sync::Mutex;
use walkdir::WalkDir;

enum PlaybackState {
    None,
    Joined(PlaybackStateJoined),
}

struct PlaybackStateJoined {
    channel_name: String,
    call: Arc<Mutex<Call>>,
}

impl PlaybackStateJoined {
    pub fn new(channel_name: String, call: Arc<Mutex<Call>>) -> Self {
        Self { channel_name, call }
    }
}

#[derive(Clone)]
struct SongData {
    path: String,
    artist: String,
    title: String,
}

fn format_song(id: usize, song_data: &SongData) -> String {
    format!("**{id}**\\. {} - {}", song_data.artist, song_data.title)
}

struct Handler {
    music_states: Arc<Mutex<HashMap<GuildId, PlaybackState>>>,
    songs: Vec<SongData>,
}

impl Handler {
    fn new(music_dir: impl AsRef<str>) -> Result<Self, String> {
        log::info!("Collecting songs");
        let mut songs = Vec::new();

        for entry in WalkDir::new(music_dir.as_ref())
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(e) => {
                    log::warn!("Failed to walk entry: {e}");
                    None
                }
            })
        {
            if entry.path().is_file() {
                let tag = match Tag::new().read_from_path(entry.path()) {
                    Ok(tag) => tag,
                    Err(e) => {
                        log::warn!("Failed to read file `{}`: {e}", entry.path().display());
                        continue;
                    }
                };

                let artist = tag
                    .artist()
                    .map_or_else(|| "[Unknown]".to_owned(), |artist| artist.to_owned());
                let title = tag
                    .title()
                    .map_or_else(|| "[Unknown]".to_owned(), |artist| artist.to_owned());

                songs.push(SongData {
                    path: entry.path().to_string_lossy().to_string(),
                    artist,
                    title,
                })
            }
        }

        log::info!("Finished collecting songs");

        Ok(Self {
            music_states: Arc::new(Mutex::new(HashMap::new())),
            songs,
        })
    }
}

enum BotCommand {
    Join,
    Leave,
    Play(Option<usize>),
    Pause,
    Resume,
    Stop,
    NowPlaying,
    ShowQueue,
    Enqueue(usize),
    Skip,
    Shuffle,
    TrackList(Option<usize>),
}

impl TryFrom<&CommandInteraction> for BotCommand {
    type Error = String;

    fn try_from(value: &CommandInteraction) -> Result<Self, String> {
        match value.data.name.as_str() {
            "join" => Ok(Self::Join),
            "leave" => Ok(Self::Leave),
            "play" => {
                if value.data.options.is_empty() {
                    Ok(Self::Play(None))
                } else {
                    Ok(Self::Play(Some(
                        value.data.options[0].value.as_i64().unwrap() as usize,
                    )))
                }
            }
            "pause" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            "stop" => Ok(Self::Stop),
            "nowplaying" => Ok(Self::NowPlaying),
            "showqueue" => Ok(Self::ShowQueue),
            "enqueue" => Ok(Self::Enqueue(
                value.data.options[0].value.as_i64().unwrap() as usize
            )),
            "skip" => Ok(Self::Skip),
            "shuffle" => Ok(Self::Shuffle),
            "tracklist" => {
                if value.data.options.is_empty() {
                    Ok(Self::TrackList(None))
                } else {
                    Ok(Self::TrackList(Some(
                        value.data.options[0].value.as_i64().unwrap() as usize,
                    )))
                }
            }
            command => Err(format!("Invalid command {command}")),
        }
    }
}

async fn send_response(text: String, ctx: &Context, interaction: &CommandInteraction) {
    if let Err(e) = interaction
        .create_response(
            ctx.http(),
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(text),
            ),
        )
        .await
    {
        log::error!("Failed to send text response: {e}");
    }
}

async fn handle_command(
    int: &CommandInteraction,
    hndl: &Handler,
    ctx: &Context,
) -> Result<(), Box<dyn Error>> {
    let http = Arc::clone(&ctx.http);
    let cache = Arc::clone(&ctx.cache);
    let guild_id = if let Some(id) = int.guild_id {
        id
    } else {
        send_response("This bot only works for guilds".to_string(), ctx, int).await;
        return Ok(());
    };

    if hndl.songs.is_empty() {
        send_response("No songs found".to_owned(), ctx, int).await;
        return Ok(());
    }

    let command = match BotCommand::try_from(int) {
        Ok(command) => command,
        Err(e) => {
            send_response(e, ctx, int).await;
            return Ok(());
        }
    };

    let mut states_lock = hndl.music_states.lock().await;
    let state = match states_lock.entry(guild_id) {
        Entry::Occupied(occupied) => occupied.into_mut(),
        Entry::Vacant(vacant) => vacant.insert(PlaybackState::None),
    };

    macro_rules! not_joined {
        () => {
            send_response(
                "No voice channels joined, use `/join` to join a voice channel".to_owned(),
                ctx,
                int,
            )
            .await;
        };
    }

    match command {
        BotCommand::Join => match &state {
            PlaybackState::None => {
                let mut voice_channel_id = None;
                for (id, channel) in guild_id.channels(http).await? {
                    if let ChannelType::Voice = channel.kind {
                        for member in channel.members(Arc::clone(&cache))? {
                            if member.user.id == int.user.id {
                                voice_channel_id = Some(id);
                            }
                        }
                    }
                }

                let connect_to = match voice_channel_id {
                    Some(channel_id) => channel_id,
                    None => {
                        send_response(
                            "You must be in a voice chat to use `/join` command".to_owned(),
                            ctx,
                            int,
                        )
                        .await;
                        return Ok(());
                    }
                };

                let channel_name = connect_to.name(ctx.http()).await?;
                send_response(format!("Joined to `{channel_name}`"), ctx, int).await;

                let manager = songbird::get(ctx).await.unwrap().clone();
                let call = manager.join(guild_id, connect_to).await?.clone();

                *state = PlaybackState::Joined(PlaybackStateJoined::new(channel_name, call))
            }
            PlaybackState::Joined(joined) => {
                send_response(
                    format!("Already joined to `{}`", joined.channel_name),
                    ctx,
                    int,
                )
                .await;
            }
        },
        BotCommand::Leave => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                send_response(format!("Leaving `{}`", joined.channel_name), ctx, int).await;

                let manager = songbird::get(ctx).await.unwrap();

                manager.remove(guild_id).await?;

                *state = PlaybackState::None;
            }
        },
        BotCommand::Play(id) => {
            match &state {
                PlaybackState::None => {
                    not_joined!();
                }
                PlaybackState::Joined(joined) => {
                    let mut call = joined.call.lock().await;

                    call.queue().stop();

                    let start_id = id.unwrap_or(0);
                    let len = hndl.songs.len();
                    if start_id >= hndl.songs.len() {
                        send_response(
                            format!("Song ID **{start_id}** is out of range. Available songs: **0**-**{}**", len - 1),
                            ctx,
                            int,
                        )
                        .await;
                        return Ok(());
                    }

                    for i in start_id..hndl.songs.len() {
                        let song_data = &hndl.songs[i];
                        let file = File::new(song_data.path.clone());
                        let input = Input::from(file);
                        let input = input
                            .make_playable_async(get_codec_registry(), get_probe())
                            .await?;
                        let mut track = Track::new(input);
                        track.user_data = Arc::new(i);
                        call.enqueue(track).await;
                    }

                    send_response(
                        format!("Playing {}", format_song(start_id, &hndl.songs[start_id])),
                        ctx,
                        int,
                    )
                    .await;
                }
            }
        }
        BotCommand::Pause => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                call.queue().pause()?;
                send_response("Paused".to_owned(), ctx, int).await;
            }
        },
        BotCommand::Resume => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                call.queue().resume()?;
                send_response("Resumed".to_owned(), ctx, int).await;
            }
        },
        BotCommand::Stop => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                call.queue().stop();
                send_response("Stopped".to_owned(), ctx, int).await;
            }
        },
        BotCommand::NowPlaying => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                if let Some(track) = call.queue().current() {
                    let id = *track.data::<usize>();
                    let song_data = &hndl.songs[id];
                    send_response(
                        format!("Now playing {}", format_song(id, song_data)),
                        ctx,
                        int,
                    )
                    .await;
                } else {
                    send_response("Not currently playing".to_owned(), ctx, int).await;
                }
            }
        },
        BotCommand::ShowQueue => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                let queue = call.queue().current_queue();
                if queue.is_empty() {
                    send_response("Queue is empty".to_owned(), ctx, int).await;
                } else {
                    let mut response = String::new();
                    let count = 10;

                    response.push_str("Current queue:\n");
                    for track in queue.iter().take(count) {
                        let id = *track.data::<usize>();
                        let song_data = &hndl.songs[id];
                        response.push_str(format_song(id, song_data).as_str());
                        response.push('\n');
                    }

                    if queue.len() > count {
                        response.push_str("...");
                    }

                    send_response(response, ctx, int).await;
                }
            }
        },
        BotCommand::Enqueue(id) => {
            match &state {
                PlaybackState::None => {
                    not_joined!();
                }
                PlaybackState::Joined(joined) => {
                    let mut call = joined.call.lock().await;

                    let len = hndl.songs.len();
                    if id < len {
                        let song_data = &hndl.songs[id];
                        let file = File::new(song_data.path.clone());
                        let input = Input::from(file);
                        let input = input
                            .make_playable_async(get_codec_registry(), get_probe())
                            .await?;
                        let mut track = Track::new(input);
                        track.user_data = Arc::new(id);
                        call.enqueue(track).await;
                        send_response(
                            format!("Enqueued track {}", format_song(id, song_data)),
                            ctx,
                            int,
                        )
                        .await;
                    } else {
                        send_response(
                        format!("Song ID **{id}** is out of range. Available songs: **0**-**{len}**"),
                        ctx,
                        int,
                    )
                    .await;
                    }
                }
            }
        }
        BotCommand::Skip => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let call = joined.call.lock().await;
                let queue = call.queue().current_queue();

                if queue.is_empty() {
                    send_response("Queue is empty".to_owned(), ctx, int).await;
                } else {
                    call.queue().skip()?;
                    let mut response = String::new();
                    response.push_str("Skipped current track");
                    if queue.len() > 1 {
                        let id = *queue[1].data::<usize>();
                        let song_data = &hndl.songs[id];
                        response
                            .push_str(format!(". Playing {}", format_song(id, song_data)).as_str())
                    }

                    send_response(response, ctx, int).await;
                }
            }
        },
        BotCommand::Shuffle => match &state {
            PlaybackState::None => {
                not_joined!();
            }
            PlaybackState::Joined(joined) => {
                let mut call = joined.call.lock().await;

                call.queue().stop();

                let mut songs_shuffled: Vec<_> = hndl.songs.iter().enumerate().collect();
                songs_shuffled.shuffle(&mut rng());
                for (i, song_data) in &songs_shuffled {
                    let file = File::new(song_data.path.clone());
                    let input = Input::from(file);
                    let input = input
                        .make_playable_async(get_codec_registry(), get_probe())
                        .await?;
                    let mut track = Track::new(input);
                    track.user_data = Arc::new(*i);
                    call.enqueue(track).await;
                }

                send_response(
                    format!(
                        "Shuffled. Playing {}",
                        // songs_shuffled is guaranteed to be non-empty at this point
                        format_song(songs_shuffled[0].0, songs_shuffled[0].1)
                    ),
                    ctx,
                    int,
                )
                .await;
            }
        },
        BotCommand::TrackList(page) => {
            let count = hndl.songs.len();
            let tpp = 20usize;
            let page = page.unwrap_or(0);
            let page_count = count / tpp + 1;
            let begin = page * tpp;
            if page >= page_count {
                send_response(
                    format!(
                        "Page number is too big. Available pages: **0**-**{}**",
                        page_count - 1
                    ),
                    ctx,
                    int,
                )
                .await;
                return Ok(());
            }
            let end = min((page + 1) * tpp, count);

            let mut response = format!(
                "Total tracks: **{}**
Available pages: **0**-**{}**
Tracks **{}**-**{}**:
",
                count,
                page_count - 1,
                begin,
                end - 1
            );
            for (i, song_data) in hndl.songs[begin..end].iter().enumerate() {
                response.push_str(format_song(i + begin, song_data).as_str());
                response.push('\n');
            }

            let response: Vec<_> = response.chars().collect();
            for chunk in response.chunks(2000) {
                let chunk: String = chunk.iter().collect();
                send_response(chunk, ctx, int).await;
            }
        }
    }

    Ok(())
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(cmd) = interaction {
            if let Err(e) = handle_command(&cmd, self, &ctx).await {
                log::warn!("Failed to handle command: {e}");
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        log::info!("{} is connected!", ready.user.name);

        log::info!("Registering commands");

        let join = CreateCommand::new("join").description("Join a voice chat");
        let leave = CreateCommand::new("leave").description("Leave a voice chat");
        let play = CreateCommand::new("play")
            .description("Play a song")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "id", "Song ID")
                    .min_int_value(0)
                    .required(false),
            );
        let stop = CreateCommand::new("stop").description("Stop playback resetting queue");
        let pause = CreateCommand::new("pause").description("Pause a song");
        let resume = CreateCommand::new("resume").description("Resume a paused song");
        let now_playing =
            CreateCommand::new("nowplaying").description("Show the song that is currently played");
        let show_queue = CreateCommand::new("showqueue").description("Show the song queue");
        let enqueue = CreateCommand::new("enqueue")
            .description("Add a song to the song queue")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "id", "Song ID")
                    .min_int_value(0)
                    .required(true),
            );
        let skip =
            CreateCommand::new("skip").description("Jump to the next track on the song queue");
        let shuffle = CreateCommand::new("shuffle").description("Shuffle songs and play");
        let track_list = CreateCommand::new("tracklist")
            .description("List all tracks")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "num", "Page Number")
                    .min_int_value(0)
                    .required(false),
            );

        let commands = vec![
            join,
            leave,
            play,
            pause,
            resume,
            stop,
            now_playing,
            show_queue,
            enqueue,
            skip,
            shuffle,
            track_list,
        ];

        for command in commands {
            if let Err(e) = Command::create_global_command(ctx.http(), command).await {
                log::warn!("Failed to register command: {e}");
            }
        }

        log::info!("Commands registered");
    }
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let token = match env::var("DISCORD_TOKEN") {
        Ok(token) => token,
        Err(e) => {
            log::error!("Failed to get discord token: {e}");
            std::process::exit(1);
        }
    };

    let music_dir = match env::var("MUSIC_DIR") {
        Ok(music_dir) => music_dir,
        Err(e) => {
            log::error!("Failed to get MUSIC_DIRECTORY: {e}");
            std::process::exit(1);
        }
    };

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES;

    let handler = match Handler::new(music_dir) {
        Ok(handler) => handler,
        Err(e) => {
            log::error!("Failed to create handler: {e}");
            std::process::exit(1);
        }
    };

    let mut client = match Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird()
        .await
    {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to create client: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = client.start().await {
        log::error!("Failed to start client: {e}");
        std::process::exit(1);
    }
}
