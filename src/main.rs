use audiotags::Tag;
use rand::{rng, seq::SliceRandom};
use serenity::{
    all::{
        ButtonStyle, CacheHttp, ChannelType, Command, CommandInteraction, CommandOptionType,
        Context, CreateActionRow, CreateButton, CreateCommand, CreateCommandOption,
        CreateInteractionResponse, CreateInteractionResponseMessage, EventHandler, GatewayIntents,
        GuildId, Interaction,
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
    fmt::Display,
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
    id: usize,
    path: String,
    artist: String,
    title: String,
}

impl Display for SongData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "**{}**\\. {} - {}", self.id, self.artist, self.title)
    }
}

#[derive(Copy, Clone)]
enum ButtonDataKind {
    TrackList,
}

impl Display for ButtonDataKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ButtonDataKind::TrackList => write!(f, "t"),
        }
    }
}

impl ButtonDataKind {
    fn from_char(value: char) -> Option<Self> {
        match value {
            't' => Some(Self::TrackList),
            _ => None,
        }
    }
}

struct ButtonData {
    kind: ButtonDataKind,
    id: char,
    page: usize,
}

impl Display for ButtonData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}{}", self.kind, self.id, self.page)
    }
}

impl ButtonData {
    fn string(kind: ButtonDataKind, id: char, page: usize) -> String {
        Self { kind, id, page }.to_string()
    }

    fn parse(data: &str) -> Option<Self> {
        if data.len() < 3 {
            None
        } else {
            let kind = ButtonDataKind::from_char(data.chars().nth(0).unwrap())?;
            let id = data.chars().nth(1).unwrap();
            let page = &data[2..];
            let Ok(page) = page.parse::<usize>() else {
                return None;
            };

            Some(Self { kind, id, page })
        }
    }
}

struct Handler {
    music_states: Arc<Mutex<HashMap<GuildId, PlaybackState>>>,
    songs: Vec<SongData>,
}

impl Handler {
    fn new(music_dir: impl AsRef<str>) -> Result<Self, String> {
        log::info!("Collecting songs");
        let mut songs = Vec::new();

        let mut id = 0usize;
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
                    id,
                    path: entry.path().to_string_lossy().to_string(),
                    artist,
                    title,
                });

                id += 1;
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
    TrackList,
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
            "tracklist" => Ok(Self::TrackList),
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

fn create_response_text(text: String) -> CreateInteractionResponseMessage {
    CreateInteractionResponseMessage::new().content(text)
}

fn create_response_song_list(
    title: String,
    kind: ButtonDataKind,
    songs: &[SongData],
    page: usize,
) -> CreateInteractionResponseMessage {
    let count = songs.len();
    let tpp = 20usize;
    let page_count = count / tpp + 1;
    let begin = page * tpp;
    let end = min((page + 1) * tpp, count);
    let page = std::cmp::min(page, page_count - 1);

    let mut response = format!("**{title}**:\n");
    for song in songs[begin..end].iter() {
        response.push_str(format!("{song}\n").as_str());
    }
    if page_count > 1 {
        response.push_str(format!("Page **{}** of **{page_count}**", page + 1).as_str());
    }

    let buttons_top = vec![
        CreateButton::new("placeholder0".to_owned())
            .label("🎵")
            .style(ButtonStyle::Secondary)
            .disabled(true),
        CreateButton::new(ButtonData::string(kind, '1', page.saturating_sub(1)))
            .label("<")
            .disabled(page == 0),
        CreateButton::new(ButtonData::string(kind, '2', page + 1))
            .label(">")
            .disabled(page == page_count - 1),
        CreateButton::new("placeholder1".to_owned())
            .label("🎵")
            .style(ButtonStyle::Secondary)
            .disabled(true),
    ];

    let buttons_bottom = vec![
        CreateButton::new(ButtonData::string(kind, '3', 0))
            .label("[")
            .disabled(page == 0),
        CreateButton::new(ButtonData::string(kind, '4', page.saturating_sub(5)))
            .label("<<")
            .disabled(page == 0),
        CreateButton::new(ButtonData::string(
            kind,
            '5',
            std::cmp::min(page + 5, page_count - 1),
        ))
        .label(">>")
        .disabled(page == page_count - 1),
        CreateButton::new(ButtonData::string(kind, '6', page_count - 1))
            .label("]")
            .disabled(page == page_count - 1),
    ];

    CreateInteractionResponseMessage::new()
        .content(response)
        .components(vec![
            CreateActionRow::Buttons(buttons_top),
            CreateActionRow::Buttons(buttons_bottom),
        ])
}

async fn handle_interaction(
    int: &Interaction,
    hndl: &Handler,
    ctx: &Context,
) -> Result<(), Box<dyn Error>> {
    let http = Arc::clone(&ctx.http);
    let cache = Arc::clone(&ctx.cache);

    match int {
        Interaction::Command(int) => {
            let guild_id = int.guild_id.unwrap();
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
                BotCommand::Play(id) => match &state {
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

                        send_response(format!("Playing {}", hndl.songs[start_id]), ctx, int).await;
                    }
                },
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
                            send_response(format!("Now playing {song_data}"), ctx, int).await;
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

                            response.push_str("**Queue**:\n");
                            for track in queue.iter().take(count) {
                                let id = *track.data::<usize>();
                                let song_data = &hndl.songs[id];
                                response.push_str(format!("{song_data}\n").as_str())
                            }

                            if queue.len() > count {
                                response.push_str("...");
                            }

                            send_response(response, ctx, int).await;
                        }
                    }
                },
                BotCommand::Enqueue(id) => match &state {
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
                            send_response(format!("Enqueued track {song_data}"), ctx, int).await;
                        } else {
                            send_response(
                                format!("Song ID **{id}** is out of range. Available songs: **0**-**{len}**"),
                                ctx,
                                int,
                            )
                            .await;
                        }
                    }
                },
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
                                response.push_str(format!(". Playing {song_data}").as_str())
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

                        let mut songs_shuffled: Vec<_> = hndl.songs.clone();
                        songs_shuffled.shuffle(&mut rng());
                        for song_data in &songs_shuffled {
                            let file = File::new(song_data.path.clone());
                            let input = Input::from(file);
                            let input = input
                                .make_playable_async(get_codec_registry(), get_probe())
                                .await?;
                            let mut track = Track::new(input);
                            track.user_data = Arc::new(song_data.id);
                            call.enqueue(track).await;
                        }

                        // songs_shuffled is guaranteed to be non-empty at this point
                        let playing = &songs_shuffled[0];
                        send_response(format!("Shuffled. Playing {playing}",), ctx, int).await;
                    }
                },
                BotCommand::TrackList => {
                    let resp = create_response_song_list(
                        "Track List".to_owned(),
                        ButtonDataKind::TrackList,
                        &hndl.songs,
                        0,
                    );
                    int.create_response(ctx.http(), CreateInteractionResponse::Message(resp))
                        .await?;
                }
            }
        }
        Interaction::Component(int) => {
            if let Some(button_data) = ButtonData::parse(&int.data.custom_id) {
                match button_data.kind {
                    ButtonDataKind::TrackList => {
                        let resp = create_response_song_list(
                            "Track List".to_owned(),
                            ButtonDataKind::TrackList,
                            &hndl.songs,
                            button_data.page,
                        );
                        int.create_response(
                            ctx.http(),
                            CreateInteractionResponse::UpdateMessage(resp),
                        )
                        .await?;
                    }
                }
            } else {
                int.create_response(
                    ctx.http(),
                    CreateInteractionResponse::UpdateMessage(create_response_text(
                        "Something went wrong".to_owned(),
                    )),
                )
                .await?;
            }
        }
        _ => {}
    }

    Ok(())
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Err(e) = handle_interaction(&interaction, self, &ctx).await {
            log::warn!("Failed to handle command: {e}");
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
        let track_list = CreateCommand::new("tracklist").description("List all tracks");

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
