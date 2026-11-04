const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* Copy the install command. The button reports what actually happened: a failed
 * clipboard write says so rather than claiming success, because the user's next
 * move is a paste that would otherwise silently produce nothing. */
document.querySelectorAll("[data-copy]").forEach((button) => {
  const original = button.textContent.trim();
  button.addEventListener("click", async () => {
    const value =
      button.dataset.copyValue ||
      document
        .getElementById(button.dataset.copy)
        ?.textContent.replace(/^\$\s*/, "")
        .trim() ||
      "";

    let ok = true;
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      ok = false;
    }

    button.textContent = ok ? "Copied" : "Press ⌘C";
    button.dataset.copied = String(ok);

    if (ok && !reduceMotion) {
      button.animate([{ opacity: 0.55 }, { opacity: 1 }], {
        duration: 260,
        easing: "cubic-bezier(0.16, 1, 0.3, 1)",
      });
    }

    setTimeout(() => {
      button.textContent = original;
      delete button.dataset.copied;
    }, 1400);
  });
});

/* Everything below is decoration over a page that is already complete. With
 * reduced motion, none of it runs and the pictures stay as authored. */
if (!reduceMotion && "IntersectionObserver" in window) {
  document.documentElement.classList.add("motion");

  // Live loops only tick while their picture is on screen and the tab is visible.
  const visible = new WeakMap();
  const onScreen = (el) => visible.get(el) === true && !document.hidden;
  const watch = new IntersectionObserver(
    (entries) => entries.forEach((e) => visible.set(e.target, e.isIntersecting)),
    { rootMargin: "80px" },
  );

  /* Blocks ink in once as they arrive. */
  const reveal = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        e.target.classList.add("is-in");
        e.target.querySelectorAll("[data-lines]").forEach((el) => el.classList.add("is-printed"));
        reveal.unobserve(e.target);
      }
    },
    { rootMargin: "0px 0px -12% 0px", threshold: 0.05 },
  );
  document.querySelectorAll("[data-reveal]").forEach((el) => reveal.observe(el));
  document.querySelectorAll("[data-lines]").forEach((el) => {
    [...el.children].forEach((line, i) => line.style.setProperty("--i", String(i)));
  });

  /* Spinners: the glyph Claude Code cycles while it works. */
  const SPIN = ["✻", "✽", "✶", "✳", "✢", "·", "✢", "✳", "✶", "✽"];
  let frame = 0;
  setInterval(() => {
    frame = (frame + 1) % SPIN.length;
    for (const el of document.querySelectorAll("[data-spin]")) {
      if (onScreen(el.closest("[data-stream], [data-gods]"))) el.textContent = SPIN[frame];
    }
  }, 140);

  /* The hero worker streams: lines arrive one at a time, then the turn starts over. */
  const stream = document.querySelector("[data-stream]");
  if (stream) {
    watch.observe(stream);
    const lines = stream.innerHTML.split("\n");
    let shown = lines.length;
    const paint = () => {
      stream.innerHTML = lines
        .slice(0, shown)
        .map((l, i) => (i === shown - 1 ? `<span class="stream-line">${l}</span>` : l))
        .join("\n");
    };
    setInterval(() => {
      if (!onScreen(stream)) return;
      shown = shown >= lines.length + 3 ? 3 : shown + 1;
      if (shown <= lines.length) paint();
    }, 900);
  }

  /* The task bar types a command, holds it, clears it, types the next. */
  const typed = document.querySelector("[data-type]");
  if (typed) {
    watch.observe(typed);
    const commands = ["/plan ship billing v2", "/gam rewrite the retry logic", "fix the login redirect", "/sync"];
    let c = 0;
    let n = commands[0].length;
    let hold = 18;
    setInterval(() => {
      if (!onScreen(typed)) return;
      const cmd = commands[c];
      if (n < cmd.length) n += 1;
      else if (hold > 0) hold -= 1;
      else {
        c = (c + 1) % commands.length;
        n = 0;
        hold = 18;
      }
      typed.textContent = commands[c].slice(0, n);
    }, 85);
  }

  /* God's view: the highlight walks the grid the way the arrow keys move it. */
  const gods = document.querySelector("[data-gods]");
  if (gods) {
    watch.observe(gods);
    const tiles = [...gods.querySelectorAll(".tile")];
    const order = [0, 1, 2, 5, 4, 3];
    let at = 0;
    setInterval(() => {
      if (!onScreen(gods)) return;
      const live = tiles.filter((t) => t.offsetParent !== null);
      if (live.length < 2) return;
      at = (at + 1) % order.length;
      let next = tiles[order[at]];
      if (!live.includes(next)) next = live[at % live.length];
      tiles.forEach((t) => t.classList.toggle("is-hot", t === next));
    }, 2200);
  }
}
