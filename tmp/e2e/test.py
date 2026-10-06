"""Drives the real app over WebDriver. Run via run.sh. Args: scenario names (default: all)."""
import json, os, subprocess, sys, time, traceback, wave
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.keys import Keys
from selenium.webdriver.common.options import ArgOptions
from selenium.common.exceptions import WebDriverException

E = "/home/v/s/other/cluers/tmp/e2e"
R = E + "/run"
FIX = "/home/v/s/other/cluers/src-tauri/tests/fixtures/vad/pauses_48000.wav"
MOCK = "http://127.0.0.1:18765"
SYS, MIC_SINK, MIC_SRC = "cluers_e2e_sys", "cluers_e2e_mic", "cluers_e2e_micsrc"
T0 = time.time()


def say(*a):
    print(f"[{time.time() - T0:7.2f}]", *a, flush=True)


def clip(name, start, end):
    """Slice of the `pauses` fixture (u1 1.0-3.6, u2 4.1-7.0, u3 8.5-11.3, u4 14.3-18.5 s)."""
    out = f"{R}/{name}.wav"
    with wave.open(FIX) as w:
        sr = w.getframerate()
        w.setpos(int(start * sr))
        frames = w.readframes(int((end - start) * sr))
        params = w.getparams()
    with wave.open(out, "wb") as o:
        o.setparams(params)
        o.writeframes(frames)
    return out


Q123 = clip("q123", 0.6, 11.6)  # u1 .4-3.0, u2 3.5-6.4, u3 7.9-10.7
U4 = clip("u4", 14.0, 18.8)  # .3-4.5
U1 = clip("u1", 0.6, 3.9)


def play(sink, wav):
    return subprocess.Popen(["paplay", "-d", sink, wav])


def mock():
    try:
        return [json.loads(l) for l in open(R + "/mock.jsonl")]
    except FileNotFoundError:
        return []


def kinds(k, since=0):
    return [r for r in mock()[since:] if r["kind"] == k]


def wait(cond, timeout, what):
    end = time.time() + timeout
    while time.time() < end:
        v = cond()
        if v:
            return v
        time.sleep(0.1)
    raise AssertionError(f"timeout ({timeout}s) waiting for {what}")


shots = 0


def shot(name):
    global shots
    shots += 1
    p = f"{R}/{shots:02d}-{name}.png"
    subprocess.run(["grim", p], check=True)
    say("screenshot", p)


opts = ArgOptions()
opts.set_capability("browserName", "wry")
opts.set_capability("tauri:options", {"application": R + "/pluely"})
d = webdriver.Remote("http://127.0.0.1:14444", options=opts)
d.implicitly_wait(0)


def main_window():
    def find():
        for h in d.window_handles:
            d.switch_to.window(h)
            try:
                if d.execute_script("return location.pathname") == "/":
                    return True
            except WebDriverException as e:  # webview still loading; retried by wait()
                say("window not ready", h, e.msg)
    wait(find, 20, "overlay window")


def btn(title):
    els = d.find_elements(By.XPATH, f"//*[@title={json.dumps(title)} or @data-original-title={json.dumps(title)}]")
    return els[0] if els else None


def click(title, timeout=10):
    e = wait(lambda: (b := btn(title)) and b.is_enabled() and b, timeout, f"button {title!r}")
    e.click()
    say("clicked", title)


def click_text(text, timeout=10):
    xp = f"//button[contains(normalize-space(.), {json.dumps(text)})]"
    e = wait(lambda: next((b for b in d.find_elements(By.XPATH, xp) if b.is_enabled()), None), timeout, f"button text {text!r}")
    e.click()
    say("clicked text", text)


def page_has(text):
    return text in d.find_element(By.TAG_NAME, "body").text


def ipc(cmd, args=None):
    return d.execute_async_script(
        "const [c,a,done]=arguments; window.__TAURI_INTERNALS__.invoke(c,a).then(r=>done({ok:r}),e=>done({err:String(e)}))",
        cmd, args or {})


def source_outputs():
    """{stream index: source name} for this app's Pulse capture streams."""
    srcs = {s["index"]: s["name"] for s in json.loads(subprocess.check_output(["pactl", "-f", "json", "list", "sources"]))}
    outs = json.loads(subprocess.check_output(["pactl", "-f", "json", "list", "source-outputs"]))
    return {o["index"]: srcs[o["source"]] for o in outs if o["properties"].get("application.process.binary") == "pluely"}


def assert_no_real_mic():
    for src in source_outputs().values():
        assert src.startswith("cluers_e2e"), f"app opened a non-test source: {src}"


AI_CURL = f"""curl {MOCK}/v1/chat/completions -H "Content-Type: application/json" -d '{{"model": "mock", "messages": [{{"role": "system", "content": "{{{{SYSTEM_PROMPT}}}}"}}, {{"role": "user", "content": "{{{{TEXT}}}}"}}]}}'"""
STT_CURL = f"""curl -X POST {MOCK}/stt -F "file={{{{AUDIO}}}}" -F "model=mock" """


def setup():
    main_window()
    wait(lambda: btn("Toggle voice input"), 20, "overlay")
    ins = ipc("get_input_devices")
    outs = ipc("get_output_devices")
    say("input devices", ins)
    say("output devices", outs)
    assert any(x["id"] == MIC_SRC for x in ins["ok"]), "virtual mic not listed"
    assert any(x["id"] == SYS for x in outs["ok"]), "sys sink not listed"
    seed = {
        "pluely_api_enabled": "false",
        "curl_custom_ai_providers": [{"id": "custom-e2e-ai", "curl": AI_CURL, "responseContentPath": "choices[0].message.content", "streaming": True, "isCustom": True}],
        "curl_custom_speech_providers": [{"id": "custom-e2e-stt", "curl": STT_CURL, "responseContentPath": "text", "streaming": False, "isCustom": True}],
        "curl_selected_ai_provider": {"provider": "custom-e2e-ai", "variables": {}},
        "curl_selected_stt_provider": {"provider": "custom-e2e-stt", "variables": {}},
        "selected_audio_devices": {"input": {"id": MIC_SRC, "name": MIC_SRC}, "output": {"id": SYS, "name": SYS}},
    }
    for k, v in seed.items():
        d.execute_script("localStorage.setItem(arguments[0], arguments[1])", k, v if isinstance(v, str) else json.dumps(v))
    for h in d.window_handles:
        d.switch_to.window(h)
        d.refresh()
    main_window()
    wait(lambda: btn("Toggle voice input"), 20, "overlay after reload")
    time.sleep(2)
    shot("seeded")


def typed_chat():
    """Baseline: text input -> mock LLM -> rendered answer."""
    n0 = len(mock())
    inp = d.find_element(By.XPATH, "//input[@placeholder='Ask me anything...']")
    inp.send_keys("hello from webdriver", Keys.ENTER)
    wait(lambda: kinds("chat", n0), 15, "chat request")
    wait(lambda: page_has("ANSWER"), 15, "answer rendered")
    shot("typed")
    d.find_element(By.TAG_NAME, "body").send_keys(Keys.ESCAPE)
    time.sleep(1)


def ptt_once(action):
    n0 = len(mock())
    click("Toggle voice input")
    wait(lambda: (b := btn("Send to AI")) and b.is_enabled(), 10, "recorder started")
    outs = source_outputs()
    say("source-outputs while recording", outs)
    assert MIC_SRC in outs.values(), f"PTT stream not on {MIC_SRC}: {outs}"
    assert_no_real_mic()
    p = play(MIC_SINK, U1)
    p.wait()
    time.sleep(0.5)
    if action == "send":
        click("Send to AI")
        stt = wait(lambda: kinds("stt", n0), 15, "stt request")
        chat = wait(lambda: kinds("chat", n0), 15, "chat request")
        say("stt", stt)
        last = chat[-1]["messages"][-1]["content"]
        assert stt[0]["text"] in json.dumps(last), f"transcript not submitted: {last}"
        assert stt[0]["rms"] > 0.01, f"recorded silence: {stt[0]}"
        wait(lambda: page_has(f"ANSWER{chat[-1]['n']}"), 15, "answer rendered")
        shot("ptt-send")
        inp = d.find_element(By.XPATH, "//input[@placeholder='Ask me anything...']")
        wait(lambda: inp.is_enabled(), 15, "completion finished")
        time.sleep(0.5)  # past the post-completion input focus (useCompletion.ts:122), which kills an open recorder: see ptt_focus
    else:
        click("Stop recording")
        time.sleep(2)
        assert not kinds("stt", n0), "discard still transcribed"
        assert MIC_SRC not in source_outputs().values(), "mic stream left open after discard"
        shot("ptt-discard")
    d.find_element(By.TAG_NAME, "body").send_keys(Keys.ESCAPE)
    time.sleep(0.5)


def ptt():
    ptt_once("send")
    ptt_once("discard")
    ptt_once("send")  # right after a discard
    ptt_once("send")  # and right after a send


def open_capture():
    click("Start system audio capture")
    wait(lambda: page_has("Auto-detect"), 15, "capture popover")
    time.sleep(1)
    outs = source_outputs()
    say("source-outputs while capturing", outs)
    assert_no_real_mic()


def stop_capture():
    click("Stop system audio capture")
    time.sleep(1)


def continuous():
    open_capture()
    click_text("Manual")
    time.sleep(1)
    for action in ["Stop & Send", "Discard", "Stop & Send"]:
        n0 = len(mock())
        click_text("Start Recording")
        p = play(SYS, Q123)
        p.wait()
        time.sleep(0.5)
        click_text(action)
        if action == "Discard":
            time.sleep(3)
            assert not kinds("stt", n0), "discarded recording was transcribed"
            shot("cont-discard")
        else:
            chat = wait(lambda: kinds("chat", n0), 20, "chat request")
            stt = kinds("stt", n0)
            say("stt", stt)
            assert len(stt) == 1 and len(chat) == 1, (stt, chat)
            assert 10 < stt[0]["secs"] < 13, stt
            wait(lambda: page_has(f"ANSWER{chat[0]['n']}"), 15, "answer rendered")
            shot("cont-send")


def quick():
    """Quick action between recordings (capture already open, manual mode, an answer showing)."""
    n0 = len(mock())
    click_text("What should I say?")
    chat = wait(lambda: kinds("chat", n0), 15, "quick action chat")
    say("quick chat last msg", chat[0]["messages"][-1])
    wait(lambda: page_has(f"ANSWER{chat[0]['n']}"), 15, "quick answer rendered")
    shot("quick")
    n1 = len(mock())
    click_text("Start Recording")
    play(SYS, U1).wait()
    time.sleep(0.5)
    click_text("Stop & Send")
    chat = wait(lambda: kinds("chat", n1), 20, "recording after quick action")
    wait(lambda: page_has(f"ANSWER{chat[0]['n']}"), 15, "answer rendered")
    n2 = len(mock())
    click_text("What should I say?")
    wait(lambda: kinds("chat", n2), 15, "second quick action")
    shot("quick-2")


def vad_case(name, sys_wav, mic_wav=None, mic_delay=0.0, settle=6):
    n0 = len(mock())
    t = time.time()
    ps = [play(SYS, sys_wav)]
    if mic_wav:
        time.sleep(mic_delay)
        ps.append(play(MIC_SINK, mic_wav))
    for p in ps:
        p.wait()
    time.sleep(settle)
    stt, chat = kinds("stt", n0), kinds("chat", n0)
    rel = lambda r: round(r["t"] - t, 2)
    say(name, "stt", [(rel(s), s["secs"], s["text"]) for s in stt])
    say(name, "chat", [(rel(c), c["messages"][-1]["content"][-200:]) for c in chat])
    shot(f"vad-{name}")
    return stt, [rel(c) for c in chat]


def vad():
    if not page_has("Auto-detect"):
        open_capture()
    click_text("Auto-detect")
    time.sleep(3)  # let the noise floor settle on silence
    # u3 ends 10.7s into Q123
    stt, chats = vad_case("pause", Q123)
    assert len(chats) == 1, f"mid-sentence pauses: want 1 answer, got {len(chats)}"
    base = chats[0]
    stt2, chats2 = vad_case("mic-closes", Q123, U4, mic_delay=10.9)
    assert len(chats2) == 1, f"want 1 answer, got {len(chats2)}"
    assert all(s["rms"] > 0 for s in stt2) and len(stt2) == len(stt), f"user speech transcribed? {stt2}"
    say(f"turn close: baseline {base - 10.7:.2f}s after speech, with mic {chats2[0] - 10.7:.2f}s")
    assert chats2[0] - 10.7 < 1.5, "mic speech did not close the turn quickly"
    stt3, chats3 = vad_case("echo", Q123, Q123, mic_delay=0.0)
    assert len(chats3) == 1, f"echo: want 1 answer, got {len(chats3)}"
    assert len(stt3) == len(stt), f"echo transcribed as extra segments: {stt3}"
    stt4, chats4 = vad_case("echo-300ms", Q123, Q123, mic_delay=0.3)
    assert len(chats4) == 1, f"delayed echo: want 1 answer, got {len(chats4)}"
    assert len(stt4) == len(stt), f"delayed echo transcribed as extra segments: {stt4}"
    assert chats4[0] - 10.7 > 1.5, f"delayed echo closed the turn early ({chats4[0] - 10.7:.2f}s)"
    stop_capture()


def action(a):
    subprocess.run([R + "/pluely", "--action", a], check=True, timeout=20)
    say("cli action", a)


def ptt_during_vad():
    """The audio_recording shortcut is ignored while a capture runs (85e7cf4): no recorder, no extra mic stream, a js_log line."""
    if not page_has("Auto-detect"):
        open_capture()
    click_text("Auto-detect")
    time.sleep(2)
    n0 = len(mock())
    before = source_outputs()
    log0 = open(R + "/app.log").read().count("push-to-talk ignored: a capture is running")
    action("audio_recording")
    time.sleep(2)
    after = source_outputs()
    log1 = open(R + "/app.log").read().count("push-to-talk ignored: a capture is running")
    say("streams before", before, "after", after, "log lines", log0, "->", log1, "recorder", bool(btn("Send to AI")))
    shot("ptt-during-vad")
    assert not btn("Send to AI"), "recorder opened during capture"
    assert after == before and list(after.values()).count(MIC_SRC) == 1, f"extra stream: {before} -> {after}"
    assert log1 == log0 + 1, "missing 'push-to-talk ignored' log line"
    assert not kinds("stt", n0)
    stop_capture()


def ptt_empty():
    """Send the instant the recorder starts, then a normal recording still works."""
    n0 = len(mock())
    click("Toggle voice input")
    b = wait(lambda: (b := btn("Send to AI")) and b.is_enabled() and b, 10, "recorder started")
    b.click()
    time.sleep(2)
    body = d.find_element(By.TAG_NAME, "body").text
    say("after instant send:", [l for l in body.splitlines() if l.strip()][:6], "stt:", kinds("stt", n0))
    shot("ptt-empty")
    if btn("Stop recording"):  # "Nothing was recorded" keeps the recorder open with the error
        click("Stop recording")
    d.find_element(By.TAG_NAME, "body").send_keys(Keys.ESCAPE)
    time.sleep(0.5)
    ptt_once("send")


def rec_streams():
    return {i for i, src in source_outputs().items() if src == MIC_SRC}


def recording():
    b = btn("Send to AI")
    return bool(b and b.is_displayed())


def start_rec(how):
    how()
    wait(lambda: (b := btn("Send to AI")) and b.is_enabled(), 10, "recorder started")
    s = wait(rec_streams, 5, "PTT Pulse stream")
    assert len(s) == 1, s
    return s


def check(item, cond, msg):
    say(f"[{item}]", "PASS" if cond else "FAIL", msg)
    focus_results[item] = ("PASS " if cond else "FAIL ") + msg


focus_results = {}


def ended(n0, what):
    """Recording gone, stream closed, nothing transcribed."""
    time.sleep(1.5)
    return not recording() and not rec_streams() and not kinds("stt", n0), f"{what}: recorder={recording()} streams={rec_streams()} stt={kinds('stt', n0)}"


def send_and_check(n0):
    play(MIC_SINK, U1).wait()
    time.sleep(0.5)
    click("Send to AI")
    stt = wait(lambda: kinds("stt", n0), 15, "stt request")
    chat = wait(lambda: kinds("chat", n0), 15, "chat request")
    wait(lambda: page_has(f"ANSWER{chat[-1]['n']}"), 15, "answer rendered")
    inp = d.find_element(By.XPATH, "//input[@placeholder='Ask me anything...']")
    wait(lambda: inp.is_enabled(), 15, "completion finished")
    time.sleep(0.5)
    ok = len(stt) == 1 and stt[0]["rms"] > 0.01 and stt[0]["secs"] > 3 and stt[0]["text"] in json.dumps(chat[-1]["messages"][-1])
    return ok, f"stt={[(x['secs'], x['rms'], x['text']) for x in stt]} chat_last={chat[-1]['messages'][-1]['content']!r}"


def ptt_focus():
    mic = lambda: click("Toggle voice input")
    inp = lambda: d.find_element(By.XPATH, "//input[@placeholder='Ask me anything...']")

    # (a) programmatic focus of the input mid-recording (what useCompletion does after a completion)
    n0 = len(mock())
    s = start_rec(mic)
    d.execute_script("arguments[0].focus()", inp())
    time.sleep(1)
    kept = recording() and rec_streams() == s
    say("(a) after focus: recorder", recording(), "streams", s, "->", rec_streams(), "active=input:", d.switch_to.active_element == inp())
    # (b) a real pointer click on the input, outside the popover
    inp().click()
    time.sleep(1)
    kept_b = recording() and rec_streams() == s
    shot("ptt-focus-b-clicked-input")
    say("(b) after click: recorder", recording(), "streams", rec_streams())
    ok, msg = send_and_check(n0)
    check("a", kept and ok, f"focus kept recording ({kept}); send after focus+click: {msg}")
    check("b", kept_b, f"pointer click on input kept recording: streams {s} -> {rec_streams() if not kept_b else s}")

    # (c) Esc ends it; next mic click records again
    n0 = len(mock())
    s = start_rec(mic)
    d.switch_to.active_element.send_keys(Keys.ESCAPE)
    ok, msg = ended(n0, "after Esc")
    s2 = start_rec(mic) if ok else set()
    shot("ptt-focus-c-restarted")
    check("c", ok and s2 and s2 != s, f"{msg}; restart stream {s} -> {s2}")
    click("Stop recording")
    time.sleep(1)

    # (d) mic button while recording discards; next click records again
    n0 = len(mock())
    s = start_rec(mic)
    play(MIC_SINK, U1).wait()
    mic()
    ok, msg = ended(n0, "after mic click")
    s2 = start_rec(mic) if ok else set()
    check("d", ok and s2 and s2 != s, f"{msg}; restart stream {s} -> {s2}")
    click("Stop recording")
    time.sleep(1)

    # (e) global shortcut path (pluely --action audio_recording via single-instance)
    n0 = len(mock())
    s = start_rec(lambda: action("audio_recording"))
    action("audio_recording")
    ok1, msg1 = ended(n0, "after 2nd action")
    s2 = start_rec(lambda: action("audio_recording")) if ok1 else set()
    ok2, msg2 = send_and_check(n0) if s2 else (False, "no restart")
    check("e", ok1 and ok2 and s2 != s, f"start {s}; {msg1}; restart {s2}; {msg2}")
    shot("ptt-focus-e")
    bad = [k for k, v in focus_results.items() if v.startswith("FAIL")]
    assert not bad, f"failed items: {bad}"


def nokeyring():
    """Run with NO_KEYRING=1 (no Secret Service on the bus): keyless templates must still work."""
    typed_chat()
    ptt_once("send")
    assert "keyring" not in open(R + "/app.log").read(), "app touched the keychain"


SCEN = {"typed": typed_chat, "ptt": ptt, "ptt_empty": ptt_empty, "continuous": continuous, "quick": quick, "vad": vad, "ptt_during_vad": ptt_during_vad, "ptt_focus": ptt_focus, "nokeyring": nokeyring}
results = {}
try:
    setup()
    for name in sys.argv[1:] or [k for k in SCEN if k != "nokeyring"]:
        say("=== scenario", name)
        try:
            SCEN[name]()
            results[name] = "PASS"
        except Exception as e:
            traceback.print_exc()
            results[name] = f"FAIL: {e}"
            try:
                shot(f"fail-{name}")
                for h in d.window_handles:
                    d.switch_to.window(h)
                    open(f"{R}/fail-{name}-{h}.html", "w").write(d.page_source)
                main_window()
            except Exception:
                traceback.print_exc()  # diagnostics only; the scenario already failed
            d.find_element(By.TAG_NAME, "body").send_keys(Keys.ESCAPE)
finally:
    say("RESULTS", json.dumps(results, indent=1))
    if focus_results:
        say("PTT_FOCUS", json.dumps(focus_results, indent=1))
    d.quit()
