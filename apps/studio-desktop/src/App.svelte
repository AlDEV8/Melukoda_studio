<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { onMount } from 'svelte';

  type State = 'Ready'|'Connecting'|'Live'|'Buffering'|'Reconnecting'|'Catching up'|'Buffer exhausted'|'Error'|'Stopped';
  type Profile = {
    id:string; name:string; mode:'srt_contribution'|'icecast'; host:string; port:number;
    stream_id:string; mount:string; username:string; secret:{service:string;account:string}|null; credential_mode:'srt_contribution'|'icecast'|null;
    tls:boolean; bitrate_kbps:number; channels:number; program_name:string; listener_stats_url?:string;
  };
  type SettingsTab = 'Profiles'|'Audio'|'Recording'|'Diagnostics'|'Application';

  let state: State = 'Ready';
  let status = 'Choose a profile and confirm the input before starting.';
  let active = 'No profile selected';
  let recordStatus = 'Not recording';
  let isRecording = false;
  let streamElapsedMs = 0;
  let recordingElapsedMs = 0;
  let markerTitle = '';
  let recordingMarkers: {id:string;at_ms:number;title:string}[] = [];
  let diagnostic = '';
  let settingsOpen = false;
  let settingsTab: SettingsTab = 'Profiles';
  let runtimeSecret = '';
  let encoder = { available: false, aac_lc: false, srt: false, icecast: false, version: '', path: '' };
  let meter = {left:-96,right:-96,leftRms:-96,rightRms:-96,clips:0};
  let meterFrames = 0;
  let meterError = '';
  let droppedSamples = 0;
  let activeInputChannels = 0;
  let activeOutputChannels = 0;
  let directMonitor = false;
  let loudnessEnabled = true;
  let loudnessTargetDbfs = -16;
  let loudnessDbfs = -96;
  let loudnessGainDb = 0;
  let limiterActive = false;
  let compactMode = false;
  let alwaysOnTop = false;
  let audioBufferMs = 50;
  let listenerCount: number|null = null;
  let listenerStatus = 'Server statistics not configured';
  let devices: {id:string;name:string;is_default:boolean;backend:string;sample_rate:number;input_channels:number;output_channels:number;supports_48khz:boolean}[] = [];
  let selectedInput = '';
  let profile: Profile = { id: 'active', name: '', mode: 'srt_contribution', host: '', port: 9000, stream_id: '', mount: '/live', username: 'source', secret: null, credential_mode:null, tls: true, bitrate_kbps: 128, channels: 2, program_name: '', listener_stats_url: '' };
  let profiles: Profile[] = [];
  const tabs: SettingsTab[] = ['Profiles','Audio','Recording','Diagnostics','Application'];

  async function call(name:string,args:Record<string,unknown>={}) {
    try { return await invoke<any>(name,args); }
    catch(error) { status=String(error); state='Error'; return null; }
  }
  function openSettings(tab:SettingsTab='Profiles') { settingsTab=tab; settingsOpen=true; }
  async function start() {
    const result=await call('start_broadcast',{profile,runtimeSecret:runtimeSecret || null});
    if(result){state=result.state;status=result.message;active=profile.name;await refreshTimers();}
  }
  async function stop() {
    state='Stopped'; status='Stopping broadcast…';
    const result=await call('stop_broadcast');
    if(result){state=result.state;status=result.message;if(result.state==='Stopped')streamElapsedMs=0;}
  }
  async function record() { const result=await call('toggle_recording'); if(result){isRecording=result.state==='Recording';recordStatus=result.message;if(isRecording){recordingMarkers=[];await refreshTimers();}else recordingElapsedMs=0;} }
  async function addMarker() { const result=await call('add_recording_marker',{title:markerTitle});if(result){recordingMarkers=[...recordingMarkers,result];markerTitle='';recordStatus=`Marker added: ${result.title}`;} }
  async function testAudio() {
    const result=await call('audio_preflight');
    if(result){meter={left:result.left,right:result.right,leftRms:result.left,rightRms:result.right,clips:result.clips};status=`Input preflight: ${result.message}`;}
  }
  async function testConnection() {
    const result=await call('connection_preflight',{profile,runtimeSecret:runtimeSecret || null});
    if(result){diagnostic=JSON.stringify(result,null,2);status=result.summary;}
  }
  async function checkEncoder() {
    const result=await call('encoder_diagnostics');
    if(result){encoder=result;if(!result.srt && profile.mode==='srt_contribution') profile.mode='icecast';diagnostic=JSON.stringify(result,null,2);status=result.available ? `Encoder found: ${result.version}` : result.version;}
  }
  function newProfile() {
    profile={ id:crypto.randomUUID(), name:'', mode:encoder.srt ? 'srt_contribution':'icecast', host:'', port:encoder.srt ? 9000:8000, stream_id:'', mount:'/live', username:'source', secret:null, credential_mode:null, tls:true, bitrate_kbps:128, channels:2, program_name:'', listener_stats_url:'' };
    runtimeSecret=''; active='No profile selected';
  }
  async function loadProfiles() {
    const result=await call('load_profiles');
    if(!result) return;
    profiles=result;
    try {
      const last=await invoke<string|null>('last_active_profile');
      const selected=profiles.find(item=>item.id===last);
      if(selected){profile={...selected,listener_stats_url:selected.listener_stats_url || ''};active=selected.name;}
    } catch { /* profile list remains usable when the marker is absent */ }
  }
  async function selectProfile(id:string) {
    const selected=profiles.find(item=>item.id===id);
    if(!selected) return;
    profile={...selected,listener_stats_url:selected.listener_stats_url || ''};runtimeSecret='';active=selected.name;
    await call('set_active_profile',{id});
  }
  async function saveProfile(clearSecret=false) {
    const result=await call('save_profile',{profile,runtimeSecret:runtimeSecret || null,clearSecret});
    if(!result) return;
    active=profile.name;status=result.message;runtimeSecret='';
    await loadProfiles();
    const saved=profiles.find(item=>item.id===profile.id);
    if(saved) profile={...saved};
    await call('set_active_profile',{id:profile.id});
  }
  async function removeSavedCredential() {
    if(!confirm('Remove the saved credential from the system keychain?')) return;
    runtimeSecret=''; await saveProfile(true);
  }
  async function duplicateProfile() {
    profile={...profile,id:crypto.randomUUID(),name:`${profile.name || 'Profile'} copy`,secret:null};
    runtimeSecret='';await saveProfile();
  }
  async function removeProfile() {
    if(!profile.id || !confirm(`Delete ${profile.name || 'this profile'}?`)) return;
    const result=await call('delete_profile',{id:profile.id});
    if(result){newProfile();await loadProfiles();status=result.message;}
  }
  async function copyDiagnostics() { const result=await call('copy_diagnostics');if(result!==null)status='Redacted diagnostics copied to clipboard.'; }
  async function loadInputs() {
    const result=await call('list_input_devices');
    if(!result) return;
    devices=result;
    let saved:string|null=null;
    try { saved=await invoke<string|null>('saved_input_device'); } catch { /* use default */ }
    const preferred=devices.find(item=>item.id===saved)||devices.find(item=>item.is_default)||devices[0];
    if(preferred){selectedInput=preferred.id;await chooseInput();}
  }
  async function loadAudioBuffer() { const result=await call('audio_buffer_ms');if(typeof result==='number')audioBufferMs=result; }
  async function applyAudioBuffer() { const result=await call('set_audio_buffer_ms',{milliseconds:audioBufferMs});if(result)status=result.message; }
  async function loadDirectMonitor() { const result=await call('direct_monitor_enabled');if(typeof result==='boolean')directMonitor=result; }
  async function applyDirectMonitor() { const result=await call('set_direct_monitor',{enabled:directMonitor});if(result)status=result.message; }
  async function loadLoudness() { const result=await call('loudness_settings');if(result){loudnessEnabled=result.enabled;loudnessTargetDbfs=result.target_dbfs;} }
  async function applyLoudness() { const result=await call('set_loudness_settings',{enabled:loudnessEnabled,targetDbfs:Number(loudnessTargetDbfs)});if(result)status=result.message; }
  async function loadAlwaysOnTop() { try { alwaysOnTop=await getCurrentWindow().isAlwaysOnTop(); } catch(error) { status=`Could not read always-on-top state: ${String(error)}`; } }
  async function applyAlwaysOnTop() { try { await getCurrentWindow().setAlwaysOnTop(alwaysOnTop);status=alwaysOnTop?'Window pinned above other windows.':'Window is no longer pinned.'; } catch(error) { status=`Could not change always-on-top: ${String(error)}`; } }
  async function chooseInput() { if(!selectedInput)return;const result=await call('select_input_device',{deviceId:selectedInput});if(result)status=result.message; }
  async function refreshMeter() {
    try { const result=await invoke<any>('meter_snapshot');meter={left:result.left_peak_dbfs,right:result.right_peak_dbfs,leftRms:result.left_rms_dbfs,rightRms:result.right_rms_dbfs,clips:result.clips};meterFrames=result.frames;meterError=result.stream_error || '';droppedSamples=result.dropped_samples || 0;activeInputChannels=result.input_channels || 0;activeOutputChannels=result.output_channels || 0;loudnessDbfs=result.loudness_dbfs ?? -96;loudnessGainDb=result.loudness_gain_db ?? 0;limiterActive=Boolean(result.limiting); }
    catch(error) { meterError=String(error); }
  }
  async function refreshBroadcast() {
    if(['Ready','Stopped','Error'].includes(state)) return;
    try { const result=await invoke<any>('broadcast_status');state=result.state;status=result.message; }
    catch(error) { state='Error';status=String(error); }
  }
  async function refreshTimers() {
    try {
      const timing=await invoke<{broadcast_elapsed_ms:number|null;recording_elapsed_ms:number|null}>('session_timing');
      streamElapsedMs=timing.broadcast_elapsed_ms ?? 0;
      recordingElapsedMs=timing.recording_elapsed_ms ?? 0;
      isRecording=timing.recording_elapsed_ms!==null;
    } catch(error) { console.warn('Could not refresh session timing',error); }
  }
  async function refreshListeners() {
    const endpoint=profile.listener_stats_url?.trim();
    if(!endpoint) { listenerCount=null; listenerStatus='Server statistics not configured'; return; }
    try {
      const response=await fetch(endpoint,{cache:'no-store'});
      if(!response.ok) throw new Error(`HTTP ${response.status}`);
      const result=await response.json();
      if(!Number.isFinite(result.listeners) || result.listeners < 0) throw new Error('Invalid listener count');
      listenerCount=Math.round(result.listeners);
      listenerStatus=`Active HLS estimate · ${result.window_seconds || 30} s window`;
    } catch(error) {
      listenerCount=null;
      listenerStatus=`Statistics unavailable: ${String(error)}`;
    }
  }
  function formatDuration(milliseconds:number) {
    const totalSeconds=Math.floor(milliseconds/1000);
    const hours=Math.floor(totalSeconds/3600);
    const minutes=Math.floor(totalSeconds/60)%60;
    const seconds=totalSeconds%60;
    return `${String(hours).padStart(2,'0')}:${String(minutes).padStart(2,'0')}:${String(seconds).padStart(2,'0')}`;
  }
  async function refreshRuntime() { await Promise.all([refreshMeter(),refreshBroadcast()]); }
  onMount(()=>{loadInputs();loadProfiles();void (async()=>{await call('start_input_meter');await loadAudioBuffer();await loadDirectMonitor();await loadLoudness();await loadAlwaysOnTop();await refreshTimers();await refreshListeners();})();checkEncoder();const runtimeTimer=window.setInterval(refreshRuntime,100);const timingTimer=window.setInterval(refreshTimers,250);const listenerTimer=window.setInterval(refreshListeners,5000);return()=>{window.clearInterval(runtimeTimer);window.clearInterval(timingTimer);window.clearInterval(listenerTimer);};});
</script>

<svelte:head><meta name="color-scheme" content="dark" /><title>Melukoda Studio</title></svelte:head>

<main class:compact={compactMode} class="studio">
  <header class="topbar">
    <div class="brand">
      <span class="mark"><span class="mark-dot"></span></span>
      <strong>Melukoda<em>Studio</em></strong>
      <small>48 kHz · XR18-ready</small>
    </div>
    <div class:live={state==='Live'} class:error={state==='Error'} class:busy={['Connecting','Buffering','Reconnecting','Catching up'].includes(state)} class="state"><i></i>{state}</div>
    <button class="settings-button icon-button" aria-label="Open settings" on:click={()=>openSettings()}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z"/><path d="M19.4 13.5c.1-.5.1-1 0-1.5l1.7-1.3a.5.5 0 0 0 .1-.7l-1.6-2.7a.5.5 0 0 0-.6-.2l-2 .8a7.4 7.4 0 0 0-1.3-.75l-.3-2.1a.5.5 0 0 0-.5-.4h-3.2a.5.5 0 0 0-.5.4l-.3 2.1c-.47.19-.9.44-1.3.75l-2-.8a.5.5 0 0 0-.6.2L5.4 9.98a.5.5 0 0 0 .1.7l1.7 1.3c-.1.5-.1 1 0 1.5l-1.7 1.3a.5.5 0 0 0-.1.7l1.6 2.7c.13.22.4.3.6.2l2-.8c.4.31.83.56 1.3.75l.3 2.1c.05.24.26.4.5.4h3.2c.24 0 .45-.16.5-.4l.3-2.1c.47-.19.9-.44 1.3-.75l2 .8c.2.08.47 0 .6-.2l1.6-2.7a.5.5 0 0 0-.1-.7l-1.7-1.3Z"/></svg>
      <span>Settings</span>
    </button>
    <button class="icon-button" aria-label={compactMode?'Expand studio view':'Switch to compact status view'} on:click={()=>compactMode=!compactMode}>
      <span>{compactMode?'Expand':'Compact'}</span>
    </button>
    <label class="pin-control" title="Keep Melukoda Studio above other windows"><input type="checkbox" bind:checked={alwaysOnTop} on:change={applyAlwaysOnTop} /> Pin on top</label>
  </header>

  {#if compactMode}
    <section class="compact-console" aria-label="Compact stream status">
      <div><span class="label">Stream</span><strong class:live={state==='Live'}>{state}</strong><small>{active}</small></div>
      <div><span class="label">Listeners</span><strong>{listenerCount ?? '—'}</strong><small>{listenerStatus}</small></div>
      <div><span class="label">Loudness · low CPU</span><strong>{loudnessDbfs.toFixed(1)} dBFS</strong><small>{loudnessEnabled ? `${loudnessGainDb >= 0?'+':''}${loudnessGainDb.toFixed(1)} dB gain${limiterActive?' · limiting':''}`:'Off'}</small></div>
      <button class={['Live','Buffering','Reconnecting','Catching up'].includes(state)?'stop':'start'} on:click={['Live','Buffering','Reconnecting','Catching up'].includes(state)?stop:start}>{['Live','Buffering','Reconnecting','Catching up'].includes(state)?'Stop':'Start'}</button>
    </section>
  {:else}
  <section class="console" aria-label="Broadcast control">
    <section class="programme panel">
      <p class="label">Server profile</p>
      <h1>{active}</h1>
      <p class="profile-detail"><span class="chip">{profile.mode==='icecast' ? 'Icecast' : 'SRT'}</span>{profile.host || 'not configured'}</p>
      <button class="subtle" on:click={()=>openSettings('Profiles')}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m9 18 6-6-6-6"/></svg>
        Configure profile
      </button>
    </section>

    <section class="transport panel">
      <p class="label">Broadcast</p>
      <div class="transport-actions">
        {#if ['Live','Buffering','Reconnecting','Catching up'].includes(state)}
          <button class="stop" on:click={stop}><svg viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg>Stop streaming</button>
        {:else}
          <button class="start" on:click={start}><svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7Z"/></svg>Start streaming</button>
        {/if}
        <button class:active-rec={isRecording} class="record-button" on:click={record}><svg viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="7"/></svg>{isRecording?'Stop recording':'Record'}</button>
      </div>
      <div class="session-timers" aria-label="Session timers">
        <div class:running={streamElapsedMs>0} class="session-timer"><span>Stream</span><strong>{formatDuration(streamElapsedMs)}</strong></div>
        <div class:running={isRecording} class="session-timer"><span>Recording</span><strong>{formatDuration(recordingElapsedMs)}</strong></div>
      </div>
      <p class="recording">{isRecording ? `${recordingMarkers.length} marker${recordingMarkers.length===1?'':'s'} added` : recordStatus}</p>
    </section>

    <section class="input panel">
      <div class="input-heading"><div><p class="label">Input</p><h2>{devices.find(item=>item.id===selectedInput)?.name || 'No input selected'}</h2></div><button class="subtle" on:click={()=>openSettings('Audio')}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 12h3l3-8 4 16 3-8h3"/></svg>
        Audio
      </button></div>
      <div class="meter-row" aria-label="Live programme input meter">
        <div class="meter"><span style={`height:${Math.max(2,100+meter.left)}%`}></span><b>L</b></div>
        <div class="meter"><span style={`height:${Math.max(2,100+meter.right)}%`}></span><b>R</b></div>
        <div class="meter-values"><strong>{meter.left.toFixed(1)} <em>/</em> {meter.right.toFixed(1)} <small>dBFS</small></strong><span>RMS {meter.leftRms.toFixed(1)} / {meter.rightRms.toFixed(1)} · clips {meter.clips}</span></div>
      </div>
    </section>
  </section>
  {/if}

  <footer class="statusline"><span>{status}</span><span>{meterFrames ? `${meterFrames} audio frames` : 'Waiting for input'}</span></footer>
</main>

{#if settingsOpen}
  <div class="settings-layer">
    <dialog open class="settings" aria-label="Melukoda Studio settings">
      <header class="settings-head"><div><p class="label">Configuration</p><h2>Settings</h2></div><button class="icon-button" aria-label="Close settings" on:click={()=>settingsOpen=false}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M6 6l12 12M18 6 6 18"/></svg>
        <span>Close</span>
      </button></header>
      <div class="settings-body"><nav aria-label="Settings sections">{#each tabs as tab}<button class:chosen={settingsTab===tab} on:click={()=>settingsTab=tab}>
          <svg class="tab-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
            {#if tab==='Profiles'}<path d="M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8Z"/><path d="M4 21c0-4 3.5-7 8-7s8 3 8 7"/>
            {:else if tab==='Audio'}<path d="M4 12h2l2-6 3 12 3-9 2 3h4"/>
            {:else if tab==='Recording'}<circle cx="12" cy="12" r="7"/><circle cx="12" cy="12" r="2.4" fill="currentColor" stroke="none"/>
            {:else if tab==='Diagnostics'}<path d="M12 3v4M12 17v4M3 12h4M17 12h4"/><circle cx="12" cy="12" r="5"/>
            {:else}<circle cx="12" cy="12" r="3"/><path d="M4.9 4.9 8 8M16 16l3.1 3.1M19.1 4.9 16 8M8 16l-3.1 3.1"/>{/if}
          </svg>
          {tab}
        </button>{/each}</nav>
        <div class="settings-content">
          {#if settingsTab==='Profiles'}
            <h3>Server profiles</h3><p class="help">A saved profile contains its address and encoder settings. Saving a password stores it in the operating-system credential vault, not the profile file.</p>
            <div class="profile-layout"><div class="profile-list"><div class="list-title">Saved profiles <button on:click={newProfile}>New</button></div>{#if profiles.length===0}<p class="muted">No saved profile yet.</p>{:else}{#each profiles as saved}<button class:chosen={saved.id===profile.id} on:click={()=>selectProfile(saved.id)}>{saved.name}<small>{saved.mode==='srt_contribution'?'SRT':'Icecast'} · {saved.host || 'host not set'}</small></button>{/each}{/if}</div>
              <div><div class="form"><label>Name<input bind:value={profile.name} placeholder="Festival radio" /></label><label>Mode<select bind:value={profile.mode}><option value="srt_contribution" disabled={!encoder.srt}>SRT contribution</option><option value="icecast" disabled={!encoder.icecast}>Icecast / Shoutcast</option></select></label><label>Host<input bind:value={profile.host} placeholder="radio.example.org" /></label><label>Port<input type="number" bind:value={profile.port} /></label>{#if profile.mode==='srt_contribution'}<label>Stream ID<input bind:value={profile.stream_id} placeholder="festival-main" /></label><label>SRT passphrase<input bind:value={runtimeSecret} type="password" autocomplete="off" placeholder={profile.secret?'Saved securely — type to replace':'Optional — only if the listener requires encryption'} /></label>{:else}<label>Mount point<input bind:value={profile.mount} placeholder="/live" /></label><label>Username<input bind:value={profile.username} /></label><label>Password<input bind:value={runtimeSecret} type="password" autocomplete="off" placeholder={profile.secret?'Saved securely — type to replace':'Required'} /></label>{/if}<label>HLS statistics URL (optional)<input bind:value={profile.listener_stats_url} placeholder="https://stream.melukoda.ee/api/listeners" /></label><label>AAC-LC bitrate<select bind:value={profile.bitrate_kbps}><option value={96}>96 kbps stereo · economy</option><option value={128}>128 kbps stereo · standard</option><option value={160}>160 kbps stereo · high quality</option><option value={192}>192 kbps stereo · very high</option><option value={256}>256 kbps stereo · maximum</option><option value={320}>320 kbps stereo · maximum bandwidth</option></select></label></div>
                <p class="muted">{profile.mode==='srt_contribution' ? (profile.secret?'SRT passphrase saved in the system vault. Leave the field empty to keep it.':'This SRT listener currently accepts an unencrypted contribution. Add a passphrase only after server-side SRT encryption is enabled.') : (profile.secret?'Credential saved in the system vault. Leave the field empty to keep it.':'No credential saved. Type one and save this profile to store it securely.')} Listener statistics are an active HLS estimate, not an exact session total.</p>
                <div class="actions"><button class="primary" on:click={()=>saveProfile()}>Save profile</button><button on:click={duplicateProfile}>Duplicate</button><button on:click={testConnection}>Test profile</button>{#if profile.secret}<button on:click={removeSavedCredential}>Remove credential</button>{/if}<button class="danger" on:click={removeProfile}>Delete</button></div>
              </div></div>
          {:else if settingsTab==='Audio'}
            <h3>Native audio, routing & loudness</h3><p class="help">The native input feeds metering, local recording, and broadcasting. On Windows, select the <strong>ASIO</strong> XR18 device for its direct low-latency 18×18 driver path. The programme stream remains stereo AAC; all hardware channels stay available to the app for routing and future multitrack capture.</p><div class="form one-column"><label>Input device<select bind:value={selectedInput} on:change={chooseInput}>{#if devices.length===0}<option value="">No native input device found</option>{:else}{#each devices as device}<option value={device.id}>{device.name} · {device.backend.toUpperCase()} · {device.sample_rate} Hz · {device.input_channels} in / {device.output_channels} out{device.supports_48khz?'':' · 48 kHz unavailable'}{device.is_default?' · system default':''}</option>{/each}{/if}</select></label><label>Audio buffer (ms)<input type="number" min="5" max="500" step="5" bind:value={audioBufferMs} /></label><label class="checkbox"><input type="checkbox" bind:checked={directMonitor} /> Direct-monitor hardware input to matching outputs (off by default)</label><label class="checkbox"><input type="checkbox" bind:checked={loudnessEnabled} /> Loudness control — low CPU RMS leveler</label><label>Target programme level (dBFS RMS)<input type="number" min="-30" max="-6" step="1" bind:value={loudnessTargetDbfs} /></label></div><div class="actions"><button on:click={applyAudioBuffer}>Apply {audioBufferMs} ms buffer</button><button on:click={applyDirectMonitor}>Apply monitor setting</button><button on:click={applyLoudness}>Apply loudness control</button><button on:click={testAudio}>Capture 500 ms test</button></div><p class="muted">Loudness: {loudnessDbfs.toFixed(1)} dBFS RMS · gain {loudnessGainDb >= 0?'+':''}{loudnessGainDb.toFixed(1)} dB{limiterActive?' · safety limiting':''}. Low CPU mode is active. Accurate EBU R128/LUFS + true-peak analysis is heavier and is not enabled. Active interface: {activeInputChannels || '—'} inputs / {activeOutputChannels || '—'} outputs. Callback queue drops: {droppedSamples}. {meterError || status}</p>
          {:else if settingsTab==='Recording'}
            <h3>Local recording & chapters</h3><p class="help">Record a 48 kHz / 16-bit WAV independently of the stream. Add a marker as each interview starts. On stop, the app writes the WAV and a matching <code>.chapters.json</code> manifest for the web player.</p><div class="recording-summary"><span class:running={isRecording} class="recording-dot"></span><strong>{isRecording?'Recording':'Not recording'}</strong><time>{formatDuration(recordingElapsedMs)}</time></div><div class="actions"><button class="primary" on:click={record}>{isRecording?'Stop recording':'Start recording'}</button></div><div class="form one-column"><label>Interview / chapter title<input bind:value={markerTitle} placeholder="Interview — Anna Saar" on:keydown={(event)=>event.key==='Enter' && addMarker()} /></label></div><div class="actions"><button on:click={addMarker} disabled={!isRecording || !markerTitle.trim()}>Add marker at current time</button></div>{#if recordingMarkers.length}<ol class="markers">{#each recordingMarkers as marker}<li><strong>{Math.floor(marker.at_ms/60000)}:{String(Math.floor(marker.at_ms/1000)%60).padStart(2,'0')}</strong> {marker.title}</li>{/each}</ol>{/if}<p class="muted">{recordStatus}</p>
          {:else if settingsTab==='Diagnostics'}
            <h3>Diagnostics</h3><p class="help">Profile testing checks the configured endpoint and performs a short silent AAC publish: Icecast verifies its source credentials; SRT verifies its real transport handshake. Copied diagnostics redact secrets.</p><div class="actions"><button class="primary" on:click={testConnection}>Test profile</button><button on:click={checkEncoder}>Check encoder</button><button on:click={copyDiagnostics}>Copy diagnostics</button></div><pre>{diagnostic || 'No diagnostic run yet.'}</pre>
          {:else}
            <h3>Application</h3><p class="help">Use Compact in the title bar for an always-readable stream status view. Pin on top keeps this native window above other applications until you turn it off. Reset removes saved profiles, saved credential references, selected input, and their associated system-vault credentials. It does not remove the application itself.</p><label class="pin-control settings-pin"><input type="checkbox" bind:checked={alwaysOnTop} on:change={applyAlwaysOnTop} /> Keep Melukoda Studio always on top</label><div class="actions"><button on:click={()=>compactMode=true}>Open compact status view</button><button class="danger" on:click={()=>confirm('Reset all saved application settings?') && call('reset_settings')}>Reset saved settings</button></div>
          {/if}
        </div>
      </div>
    </dialog>
  </div>
{/if}
