// Generates the Reliaburger SREday keynote deck. Run `npm install && npm run build`
// here; the .pptx lands next to this script. Speaker notes live in addNotes().
const pptxgen = require("pptxgenjs");
const path = require("path");

const pres = new pptxgen();
pres.layout = "LAYOUT_WIDE"; // 13.33 x 7.5
pres.author = "Miko Pawlikowski";
pres.title = "Reliaburger - SREday";

const BUN = "2A1B12";
const MUSTARD = "F2A900";
const KETCHUP = "D2381F";
const LETTUCE = "5E8F24";
const CREAM = "FFF4E0";
const INK = "2A1B12";
const MUTED = "7A6A5E";
const TINT = "FBF3E4";
const WHITE = "FFFFFF";
const H = "Arial";
const B = "Calibri";
const MONO = "Courier New";
const LOGO = path.join(__dirname, "../../../assets/images/reliaburger_logo_v1.jpg");


function text(slide, str, o) {
  slide.addText(str, Object.assign({ isTextBox: true, margin: 0, fontFace: B, color: INK, valign: "top" }, o));
}

function memeBox(slide, x, y, w, h, dark) {
  slide.addShape(pres.shapes.ROUNDED_RECTANGLE, {
    x, y, w, h, rectRadius: 0.15,
    fill: { color: dark ? "3A2A1F" : "F7F2EA" },
    line: { color: dark ? "6B5646" : "D9CDBD", width: 1.5, dashType: "dash" },
  });
  text(slide, "meme goes here", {
    x, y: y + h / 2 - 0.2, w, h: 0.4, align: "center", valign: "middle",
    fontSize: 14, italic: true, color: dark ? "8C7866" : "B3A594",
  });
}

function darkSlide() {
  const s = pres.addSlide();
  s.background = { color: BUN };
  return s;
}
function lightSlide() {
  const s = pres.addSlide();
  s.background = { color: WHITE };
  return s;
}

function kicker(slide, str, dark) {
  text(slide, str.toUpperCase(), {
    x: 0.7, y: 0.55, w: 9, h: 0.35, fontFace: H, fontSize: 13, bold: true,
    color: dark ? MUSTARD : KETCHUP, charSpacing: 3,
  });
}
function title(slide, str, dark, o) {
  text(slide, str, Object.assign({
    x: 0.7, y: 0.95, w: 11.9, h: 1.3, fontFace: H, fontSize: 36, bold: true,
    color: dark ? CREAM : INK,
  }, o || {}));
}

// Section divider: big number chip + title
function section(num, str, sub) {
  const s = darkSlide();
  s.addShape(pres.shapes.OVAL, { x: 0.7, y: 2.3, w: 1.3, h: 1.3, fill: { color: MUSTARD } });
  text(s, String(num), { x: 0.7, y: 2.3, w: 1.3, h: 1.3, align: "center", valign: "middle", fontFace: H, fontSize: 44, bold: true, color: BUN });
  text(s, str, { x: 2.4, y: 2.2, w: 10.4, h: 1.0, fontFace: H, fontSize: 40, bold: true, color: CREAM, valign: "middle" });
  if (sub) text(s, sub, { x: 2.4, y: 3.2, w: 10, h: 0.6, fontSize: 22, color: "C9B8A6" });
  return s;
}

// ---------------------------------------------------------------- 1 Title
{
  const s = darkSlide();
  s.addImage({ path: LOGO, x: 8.3, y: 1.25, w: 4.4, h: 4.4, rounding: true });
  text(s, "Reliaburger", { x: 0.7, y: 1.9, w: 7.5, h: 1.2, fontFace: H, fontSize: 66, bold: true, color: CREAM });
  text(s, "Ten years of Kubernetes lessons, in one binary", { x: 0.7, y: 3.15, w: 7.3, h: 1.0, fontSize: 26, color: MUSTARD });
  text(s, "Miko Pawlikowski  ·  SREday", { x: 0.7, y: 5.5, w: 7, h: 0.5, fontSize: 18, color: "C9B8A6" });
  s.addNotes(`[0:00, about 30 seconds]

Hi, I'm Miko.

For the next half hour I'm going to tell you about a burger. Well, sort of. Reliaburger is a container orchestrator. It's free, it's open source, and version 0.1.0 came out today.

But I'll start with the stuff that made me build it.`);
}

// ---------------------------------------------------------------- 2 Hook
{
  const s = lightSlide();
  kicker(s, "Before your first container hits production");
  title(s, "You install all of this:");
  const items = ["A distro", "A CNI", "Ingress", "cert-manager", "Prometheus", "Grafana", "Loki", "ArgoCD", "Harbor"];
  const cw = 2.25, ch = 1.0, gx = 0.3, gy = 0.3, x0 = 0.7, y0 = 2.3;
  items.forEach((it, i) => {
    const c = i % 3, r = Math.floor(i / 3);
    const x = x0 + c * (cw + gx), y = y0 + r * (ch + gy);
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: cw, h: ch, rectRadius: 0.12, fill: { color: TINT } });
    text(s, it, { x, y, w: cw, h: ch, align: "center", valign: "middle", fontFace: H, fontSize: 20, bold: true, color: INK });
  });
  text(s, "None of them is your app.", { x: 0.7, y: 6.3, w: 7.3, h: 0.6, fontSize: 26, italic: true, bold: true, color: KETCHUP });
  memeBox(s, 8.6, 2.3, 4.0, 3.6);
  s.addNotes(`[0:30, about 1 minute]

Hands up if you've ever had to set up Kubernetes for real. Not a tutorial, the real thing that someone's going to page you about.

Keep them up if this list looks familiar. A distro, a CNI, an ingress controller, cert-manager, Prometheus, Grafana, Loki, ArgoCD, Harbor.

That's nine things. Each has its own upgrade cycle, its own config language and its own ways of breaking.

And none of them is your app. You haven't deployed anything yet.

[MEME idea: "This is fine" dog, or the Drake meme: "running my app" / "running nine things so I can run my app".]`);
}

// ---------------------------------------------------------------- 3 Section: who am I
section(1, "Who's this guy?", "And why should you listen to him");

// ---------------------------------------------------------------- 4 Late 2015
{
  const s = lightSlide();
  kicker(s, "Late 2015");
  title(s, "We're evaluating Kubernetes 1.0");
  text(s, [
    { text: "Small. Promising.", options: { breakLine: true } },
    { text: "Nobody knows what a Pod is yet.", options: {} },
  ], { x: 0.7, y: 2.5, w: 6.5, h: 2, fontSize: 28, color: MUTED, paraSpaceAfter: 10 });
  memeBox(s, 7.8, 1.9, 4.8, 4.2);
  s.addNotes(`[about 30 seconds]

Late 2015. Kubernetes 1.0 is a few months old, and we're evaluating it at work.

It's small. It's promising. You can read most of it in a weekend. Nobody really knows what a Pod is yet, including, I suspect, some of the people who wrote it.

[MEME idea: a 2015-era photo, a "baby Kubernetes" picture, or the kubectl logo from back then.]`);
}

// ---------------------------------------------------------------- 5 KubeCon 2016
{
  const s = lightSlide();
  kicker(s, "March 2016");
  title(s, "The first European KubeCon was 20 metres from our office");
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y: 2.7, w: 5.6, h: 2.6, rectRadius: 0.15, fill: { color: TINT } });
  text(s, "20m", { x: 0.7, y: 2.8, w: 5.6, h: 1.6, align: "center", valign: "middle", fontFace: H, fontSize: 96, bold: true, color: KETCHUP });
  text(s, "office door to KubeCon, London", { x: 0.7, y: 4.4, w: 5.6, h: 0.5, align: "center", fontSize: 18, color: MUTED });
  text(s, "Kubernetes 1.2 shipped the week after.", { x: 0.7, y: 5.8, w: 6, h: 0.5, fontSize: 20, color: INK });
  memeBox(s, 7.0, 2.7, 5.6, 3.6);
  s.addNotes(`[about 45 seconds]

Then in March 2016 the first European KubeCon turned up in London. About 20 metres from our office. We literally walked over.

1.2 came out the week after.

So I've been on the inside for most of Kubernetes' life. I watched it grow from something you could hold in your head to something nobody can.

Remember that bit, because it's kind of the whole talk.

[Photo slot: a real photo from KubeCon London 2016 if you have one. Otherwise a map screenshot with a "20m" arrow.]`);
}

// ---------------------------------------------------------------- 6 Platform teams + mini book
{
  const s = darkSlide();
  kicker(s, "The years after", true);
  title(s, "Platform teams, pagers, and a list of scars", true);
  const rows = [
    ["Ran platform teams", "on top of Kubernetes"],
    ["Upgrades, outages, YAML reviews", "I've been the person who says “it depends”"],
    ["Wrote a mini book", "Everything Wrong with Kubernetes"],
  ];
  rows.forEach((r, i) => {
    const y = 2.5 + i * 1.3;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.1, w: 0.6, h: 0.6, fill: { color: MUSTARD } });
    text(s, String(i + 1), { x: 0.7, y: y + 0.1, w: 0.6, h: 0.6, align: "center", valign: "middle", fontFace: H, fontSize: 20, bold: true, color: BUN });
    text(s, r[0], { x: 1.6, y, w: 6, h: 0.5, fontFace: H, fontSize: 24, bold: true, color: CREAM });
    text(s, r[1], { x: 1.6, y: y + 0.5, w: 6, h: 0.5, fontSize: 18, italic: i === 2, color: "C9B8A6" });
  });
  memeBox(s, 8.3, 2.4, 4.3, 3.8, true);
  s.addNotes(`[about 1 minute]

After that I ran platform teams on top of Kubernetes. Upgrades, outages, reviewing other people's YAML. I've been the person in the meeting who says "well, it depends".

And for the last few years I've been writing a mini book called Everything Wrong with Kubernetes.

It's not a hate letter. Kubernetes works, and millions of containers run on it every day. It's more a list of scars. Here are a few.

[MEME idea: the "I have trust issues" meme, or a pager going off at 3am.]`);
}

// ---------------------------------------------------------------- 7 Section: what's wrong
section(2, "Everything wrong with Kubernetes", "The five-minute version");

// ---------------------------------------------------------------- 8 Too many concepts
{
  const s = lightSlide();
  kicker(s, "Scar #1");
  title(s, "How many objects does it take to run a web app?");
  const objs = ["Deployment", "ReplicaSet", "Pod", "Service", "Endpoints", "Ingress", "ConfigMap", "Secret", "HPA", "PVC", "ServiceAccount"];
  objs.forEach((o, i) => {
    const c = i % 4, r = Math.floor(i / 4);
    const x = 0.7 + c * 2.95, y = 2.5 + r * 0.95;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 2.7, h: 0.7, rectRadius: 0.35, fill: { color: TINT } });
    text(s, o, { x, y, w: 2.7, h: 0.7, align: "center", valign: "middle", fontFace: MONO, fontSize: 18, bold: true, color: INK });
  });
  text(s, "…and that's before you've written a CRD.", { x: 0.7, y: 5.6, w: 11, h: 0.6, fontSize: 24, italic: true, color: KETCHUP });
  s.addNotes(`[about 1 minute]

Scar number one: the concepts.

Say I want to run a web app. Three copies, a health check, a domain name. Here's what I end up touching: a Deployment, which makes a ReplicaSet, which makes Pods. A Service and its Endpoints. An Ingress. A ConfigMap and a Secret. An HPA if I want it to scale.

That's before anyone writes a CRD. And someone always writes a CRD.

None of these is hard on its own. The trouble is that you need all of them in your head at once, just to say "run my web app".`);
}

// ---------------------------------------------------------------- 9 Batteries sold separately
{
  const s = lightSlide();
  kicker(s, "Scar #2");
  title(s, "Batteries sold separately");
  text(s, "57%", { x: 0.7, y: 2.3, w: 5.5, h: 2.0, fontFace: H, fontSize: 120, bold: true, color: KETCHUP });
  text(s, "of Kubernetes users run more than 11 separate components", { x: 0.7, y: 4.4, w: 5.3, h: 1.0, fontSize: 22, color: INK });
  text(s, "Spectro Cloud, 2024", { x: 0.7, y: 5.5, w: 5, h: 0.4, fontSize: 12, color: MUTED });
  memeBox(s, 7.0, 2.3, 5.6, 3.8);
  s.addNotes(`[about 1 minute]

Scar number two: batteries sold separately.

More than half of Kubernetes users run more than eleven separate components. Every one of them has its own release schedule and its own compatibility matrix.

So your platform team spends its time on the compatibility matrix rather than on your product. I know, because that was my job.

"Just use k3s!" I love k3s, it's a great piece of engineering. But it makes Kubernetes smaller, not simpler. Same API, same YAML, same learning curve. You've shrunk the binary. The concepts are exactly the same size.

[MEME idea: the "One does not simply upgrade Kubernetes" meme, or a toddler surrounded by LEGO pieces.]`);
}

// ---------------------------------------------------------------- 11 Pet control plane
{
  const s = lightSlide();
  kicker(s, "Scar #3");
  title(s, "The control plane is a pet");
  text(s, [
    { text: "Lose etcd quorum and you're restoring from backup.", options: { breakLine: true } },
    { text: "At 3am. Hoping the backup works.", options: {} },
  ], { x: 0.7, y: 2.5, w: 6.3, h: 2, fontSize: 26, color: INK, paraSpaceAfter: 14 });
  memeBox(s, 7.6, 2.2, 5.0, 4.0);
  s.addNotes(`[about 45 seconds]

Scar number three. We've spent ten years telling people to treat servers like cattle, not pets. Then we built a control plane that's the most pampered pet in the building.

If you lose etcd quorum, you're restoring from a backup. At 3am. Hoping somebody tested that backup.

Your apps are probably still running, but you can't touch them.

[MEME idea: a cat wearing a crown labelled "etcd".]`);
}

// ---------------------------------------------------------------- 12 Debugging archaeology
{
  const s = lightSlide();
  kicker(s, "Scar #4");
  title(s, "“Why can't A talk to B?”");
  const steps = ["DNS", "Endpoints", "kube-proxy", "iptables", "CNI logs", "NetworkPolicy", "¯\\_(ツ)_/¯"];
  steps.forEach((st, i) => {
    const x = 0.7 + i * 1.75;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y: 3.0, w: 1.55, h: 1.1, rectRadius: 0.12, fill: { color: i === steps.length - 1 ? KETCHUP : TINT } });
    text(s, st, { x, y: 3.0, w: 1.55, h: 1.1, align: "center", valign: "middle", fontFace: H, fontSize: 13, bold: true, color: i === steps.length - 1 ? WHITE : INK });
  });
  text(s, "Debugging is archaeology.", { x: 0.7, y: 4.9, w: 11, h: 0.6, fontSize: 26, italic: true, color: KETCHUP });
  s.addNotes(`[about 45 seconds]

Scar number four. Someone pings you: "why can't service A talk to service B?"

So you go digging. DNS. Endpoints. kube-proxy. iptables rules, thousands of them. CNI logs. NetworkPolicy. Then shrug.

It's archaeology. You're brushing dust off layers of other people's decisions.`);
}

// ---------------------------------------------------------------- 13 Stop complaining
{
  const s = darkSlide();
  text(s, "At some point you have to stop writing about it", { x: 0.7, y: 2.0, w: 7.2, h: 2.0, fontFace: H, fontSize: 40, bold: true, color: CREAM });
  text(s, "and build the thing.", { x: 0.7, y: 4.1, w: 7.2, h: 0.9, fontFace: H, fontSize: 40, bold: true, color: MUSTARD });
  memeBox(s, 8.4, 1.8, 4.2, 3.9, true);
  s.addNotes(`[about 30 seconds]

So I kept adding chapters to the book. And at some point you have to stop complaining and build the thing.

The problem was that I knew what I wanted, but "a container orchestrator" isn't a weekend project. It's a decade of work for a team. I had evenings.

And then AI happened.

[MEME idea: "Talk is cheap. Show me the code." (Linus), or "Old man yells at cloud".]`);
}

// ---------------------------------------------------------------- 14 Section: AI
section(3, "Then AI happened", "What building it with Claude and Codex was actually like");

// ---------------------------------------------------------------- 15 Timeline
{
  const s = lightSlide();
  kicker(s, "2026");
  title(s, "How it went");
  const tl = [
    ["Feb", "Whitepaper and design docs", "Claude pokes holes in my ideas"],
    ["Apr", "Vibe coding after work", "Evenings and weekends"],
    ["Summer", "Codex joins", "Two models, same rules"],
    ["Today", "v0.1.0", "Released this morning"],
  ];
  const lineY = 3.35;
  s.addShape(pres.shapes.LINE, { x: 1.9, y: lineY, w: 9.5, h: 0, line: { color: "D9CDBD", width: 3 } });
  tl.forEach((t, i) => {
    const cx = 1.9 + i * (9.5 / 3);
    const last = i === tl.length - 1;
    s.addShape(pres.shapes.OVAL, { x: cx - 0.3, y: lineY - 0.3, w: 0.6, h: 0.6, fill: { color: last ? KETCHUP : MUSTARD } });
    text(s, t[0], { x: cx - 1.4, y: 2.25, w: 2.8, h: 0.5, align: "center", fontFace: H, fontSize: 22, bold: true, color: last ? KETCHUP : INK });
    text(s, t[1], { x: cx - 1.4, y: 3.9, w: 2.8, h: 0.8, align: "center", fontFace: H, fontSize: 18, bold: true, color: INK });
    text(s, t[2], { x: cx - 1.4, y: 4.7, w: 2.8, h: 0.8, align: "center", fontSize: 15, color: MUTED });
  });
  s.addNotes(`[about 1 minute]

Here's how it actually went.

February: I didn't write any code. I spent the first couple of months on a whitepaper and design docs, with Claude poking holes in the ideas. "What happens when the leader dies halfway through a deploy?" "How does this work at ten thousand nodes?" It's a really good sparring partner for that.

April: I started coding. Or vibe coding, if you like. After work, evenings and weekends.

Later Codex joined in. Two models, same rules. I'll get to the rules in a minute.

And today: v0.1.0.

[CHECK: adjust the "Summer" label to the month Codex actually came in.]`);
}

// ---------------------------------------------------------------- 17 More tiring
{
  const s = darkSlide();
  kicker(s, "The job changed", true);
  text(s, "Reviewing is more tiring than writing", { x: 0.7, y: 1.0, w: 11.9, h: 1.0, fontFace: H, fontSize: 40, bold: true, color: CREAM });
  const pts = [
    ["The job moved", "From writing code to picking ideas, setting direction and reviewing"],
    ["It hasn't cemented", "I'm reading code I never had time to understand"],
    ["I forget between sessions", "Every evening starts with “wait, how does this work?”"],
    ["It never stops", "There's always another diff waiting"],
  ];
  pts.forEach((p, i) => {
    const y = 2.3 + i * 1.12;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.08, w: 0.5, h: 0.5, fill: { color: MUSTARD } });
    text(s, p[0], { x: 1.5, y, w: 10, h: 0.5, fontFace: H, fontSize: 24, bold: true, color: MUSTARD });
    text(s, p[1], { x: 1.5, y: y + 0.5, w: 10, h: 0.5, fontSize: 19, color: "C9B8A6" });
  });
  s.addNotes(`[about 1 minute 45 seconds]

The biggest surprise wasn't the code. It was what happened to my job.

Before, most of my head was in the code. Typing was the slow bit, and the design sort of happened while I typed. Now I barely write code. My job is picking the right ideas, keeping the direction straight, and then reviewing. Lots and lots of reviewing.

You'd think that's easier. It isn't. Reviewing is more tiring than writing. At least for me.

When you write code yourself, it cements in your brain. You struggled with it, so you remember it. When you review code, you're reading something that never had the time to settle in.

And I forget a lot between sessions. I'd sit down on Tuesday evening and the first twenty minutes would be "wait, how does this bit work again?" Code I'd approved on Sunday.

Also, it never stops. The machine doesn't get tired. There's always another diff.

So if anyone tells you AI makes this effortless: it moves the effort, it doesn't remove it.`);
}

// ---------------------------------------------------------------- 18 Review evolution
{
  const s = lightSlide();
  kicker(s, "As the models got better");
  title(s, "How I reviewed, month by month");
  const steps = [
    ["Every line", "Month one"],
    ["Scanning every line", "Getting comfortable"],
    ["Just the tests", "Tests are the spec"],
    ["Just end to end", "Does it actually work?"],
  ];
  steps.forEach((st, i) => {
    const w = 2.75, h = 1.5 + i * 0.5;
    const x = 0.7 + i * 3.05, y = 6.3 - h;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w, h, rectRadius: 0.12, fill: { color: [TINT, "F8DFA0", MUSTARD, KETCHUP][i] } });
    text(s, st[0], { x: x + 0.25, y: y + 0.2, w: w - 0.5, h: 0.75, fontFace: H, fontSize: 19, bold: true, color: i === 3 ? WHITE : INK });
    text(s, st[1], { x: x + 0.25, y: y + 1.0, w: w - 0.5, h: 0.4, fontSize: 15, color: i === 3 ? WHITE : MUTED });
  });
  s.addNotes(`[about 1 minute]

How I reviewed changed a lot over those months, mostly because the models kept getting better.

The first month I read every single line. Then I was scanning every line. Then I was mostly reading the tests, because the tests are the spec. If the tests say the right thing and they pass, I don't need to read every line of the implementation.

These days I mostly test end to end. Does it actually work on a real cluster?

That last step only works if you've set things up right. Which brings me to what I learned.`);
}

// ---------------------------------------------------------------- 20 Rule 1+2
{
  const s = lightSlide();
  kicker(s, "Rules 1 and 2");
  title(s, "Think first. Know your domain.");
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y: 2.4, w: 5.8, h: 3.8, rectRadius: 0.15, fill: { color: TINT } });
  text(s, "Iterate hard on the abstraction", { x: 1.1, y: 2.7, w: 5, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: LETTUCE });
  text(s, "Two months of design docs before any code. Changing an idea is cheap. Changing 200k lines built on the wrong idea isn't.",
    { x: 1.1, y: 3.3, w: 5, h: 2.6, fontSize: 19, color: INK });
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 6.8, y: 2.4, w: 5.8, h: 3.8, rectRadius: 0.15, fill: { color: TINT } });
  text(s, "Know what you're talking about", { x: 7.2, y: 2.7, w: 5, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: "B07A00" });
  text(s, "“Build me Kubernetes” gets you a nice demo. The hard part is the failure modes, and you only spot those if you've lived through them.",
    { x: 7.2, y: 3.3, w: 5, h: 2.6, fontSize: 19, color: INK });
  s.addNotes(`[about 1 minute 15 seconds]

Rule one: iterate hard on the right abstraction. That's why I spent two months on design docs before writing any code. Changing an idea in a doc costs you an evening. Changing two hundred thousand lines built on the wrong idea costs you the project. The models will happily build on whatever you give them, good or bad, and they'll do it fast. So a bad abstraction gets you to a dead end faster.

Rule two: know what you're talking about. You've all seen the videos: "I built a Kubernetes clone in one prompt". Sure. It's a demo. It runs a container on the happy path.

What happens when the leader dies halfway through a rolling deploy? When a node comes back after a partition with stale state? When the disk fills up? That's where all the real work is. And you only know to ask about it if you've been paged for it. Ten years of scars turned out to be the most useful thing I brought.`);
}

// ---------------------------------------------------------------- 21 Rule 3
{
  const s = lightSlide();
  kicker(s, "Rule 3");
  title(s, "Set up the rules before the code");
  const chain = [["Tests first", "Always. No exceptions."], ["CI", "A way to run them that isn't “trust me”"], ["End-to-end test", "Does almost nothing on day one. Grows every week."]];
  chain.forEach((c, i) => {
    const x = 0.7 + i * 4.15;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y: 2.5, w: 3.7, h: 2.2, rectRadius: 0.15, fill: { color: i === 2 ? KETCHUP : TINT } });
    text(s, c[0], { x: x + 0.3, y: 2.8, w: 3.1, h: 0.6, fontFace: H, fontSize: 24, bold: true, color: i === 2 ? WHITE : INK });
    text(s, c[1], { x: x + 0.3, y: 3.5, w: 3.1, h: 1.1, fontSize: 17, color: i === 2 ? WHITE : MUTED });
    if (i < 2) text(s, "→", { x: x + 3.72, y: 3.25, w: 0.4, h: 0.6, align: "center", fontSize: 30, bold: true, color: MUSTARD });
  });
  text(s, "Green unit tests lie. The end-to-end run caught what they missed.", { x: 0.7, y: 5.3, w: 11.9, h: 0.6, fontSize: 22, italic: true, color: KETCHUP });
  s.addNotes(`[about 1 minute 15 seconds]

Rule three: set up the rules before the code.

Tests first, always. The model writes the failing test, I read the test, and then it makes the test pass. If I only have time to read one thing, I read the test.

Then CI, so running the tests isn't a matter of trust. "I ran the tests and they all pass" is the model's favourite sentence, and it's not always true.

Then an end-to-end test that barely does anything on day one. Start a node, deploy one thing, check it's up. Every week it does a bit more.

Here's the SRE lesson. At one point an audit found a bunch of features that had great unit tests, all green, and were never actually wired into the running system. The library worked. Nothing called it. The end-to-end runs are what caught it.

Turns out AI makes the same mistakes as my old teams, just faster.`);
}

// ---------------------------------------------------------------- 22 Rule 4
{
  const s = darkSlide();
  kicker(s, "Rule 4", true);
  text(s, "Fight complexity creep", { x: 0.7, y: 1.0, w: 7.6, h: 1.0, fontFace: H, fontSize: 40, bold: true, color: CREAM });
  text(s, [
    { text: "Every model regularly wanted to 10x the codebase.", options: { breakLine: true } },
    { text: "Write it from scratch instead of using a library.", options: { breakLine: true } },
    { text: "Add a layer “just in case”.", options: {} },
  ], { x: 0.7, y: 2.4, w: 7.2, h: 2.4, fontSize: 22, color: "C9B8A6", paraSpaceAfter: 12 });
  text(s, "Just like a hotshot new grad.", { x: 0.7, y: 5.0, w: 7.4, h: 0.7, fontSize: 28, bold: true, italic: true, color: MUSTARD });
  memeBox(s, 8.4, 1.4, 4.2, 4.6, true);
  s.addNotes(`[about 1 minute]

Rule four: fight complexity creep. Hard.

Every model I used regularly wanted to 10x the codebase. Why use a well-tested crate when you could write your own? Why not add an abstraction layer, just in case? Why fix the bug when you could redesign the subsystem?

It's exactly like a brilliant new grad. Very smart, very fast, and no scar tissue. They haven't yet had to maintain the clever thing at 3am.

So a big part of my job was saying no. Use the library. Do the simple thing. Delete that.

[MEME idea: "Galaxy brain" expanding brain meme, or the "Is this a pigeon?" meme: "Is this a new abstraction layer?"]`);
}

// ---------------------------------------------------------------- L1 Standing on shoulders
{
  const s = lightSlide();
  kicker(s, "Rule 4, in practice");
  title(s, "What I didn't have to write");
  const libs = [
    ["tokio", "Async runtime", "Threads, timers, the lot"],
    ["openraft", "Raft consensus", "The hardest algorithm here"],
    ["rustls + ring", "TLS and crypto", "Never write your own"],
    ["rcgen + x509-parser", "The PKI", "Certificates are a swamp"],
    ["redb", "Embedded store", "Durable Raft log on disk"],
    ["datafusion", "SQL engine", "That's how you query metrics"],
    ["axum + hyper", "HTTP", "API, dashboards, registry"],
    ["ratatui", "Terminal UI", "The TUI, for free"],
    ["aya", "eBPF loader", "Kernel plumbing from Rust"],
    ["age", "Encryption", "Secrets at rest"],
    ["serde + clap", "Config and CLI", "Boring. Perfect."],
    ["oci-distribution", "Registry client", "Speaks Docker Hub"],
  ];
  libs.forEach((l, i) => {
    const c = i % 4, r = Math.floor(i / 4);
    const x = 0.7 + c * 3.05, y = 2.2 + r * 1.5;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 2.8, h: 1.32, rectRadius: 0.12, fill: { color: TINT } });
    text(s, l[0], { x: x + 0.22, y: y + 0.15, w: 2.4, h: 0.4, fontFace: MONO, fontSize: 14, bold: true, color: KETCHUP });
    text(s, l[1], { x: x + 0.22, y: y + 0.55, w: 2.4, h: 0.35, fontFace: H, fontSize: 15, bold: true, color: INK });
    text(s, l[2], { x: x + 0.22, y: y + 0.92, w: 2.4, h: 0.35, fontSize: 12, color: MUTED });
  });
  text(s, [
    { text: "57 direct dependencies. 708 crates in the lockfile. ", options: { bold: true, color: INK } },
    { text: "All of it maintained by someone else.", options: { color: MUTED } },
  ], { x: 0.7, y: 6.6, w: 11.9, h: 0.5, fontSize: 19 });
  s.addNotes(`[about 1 minute 30 seconds]

Rule four was fighting complexity creep. Here's the other half of that: most of this system is other people's code, and that's the point.

tokio does the async runtime. openraft does Raft, which is the single hardest algorithm in here. rustls and ring do TLS and crypto, and you never, ever write your own crypto. rcgen and x509-parser handle certificates, which are a swamp. redb is the embedded store that holds the Raft log durably.

datafusion is my favourite. That's a whole SQL engine, and it's why you can query your metrics with SQL instead of learning another query language.

axum and hyper do HTTP. ratatui gives us the terminal UI basically for free. aya loads the eBPF programs from Rust. age encrypts the secrets. serde and clap do config and the command line, and they're boring, which is exactly what you want.

57 direct dependencies, 708 crates once you count everything underneath. All maintained by someone who cares more about that problem than I do.

This is where the model and I disagreed most often. It would cheerfully offer to write a TLS handshake, or a little Raft, or its own metrics query language. Every single time, the answer was no. Use the library.`);
}

// ---------------------------------------------------------------- L2 What we did write
{
  const s = darkSlide();
  kicker(s, "The other side", true);
  text(s, "So what's actually ours?", { x: 0.7, y: 1.0, w: 11.9, h: 0.9, fontFace: H, fontSize: 36, bold: true, color: CREAM });
  const ours = [
    ["Mustard", "SWIM gossip. There's a crate for it. We wrote our own anyway, and had to justify it."],
    ["Meat", "The scheduler. Placement is the one thing nobody can do for you."],
    ["Onion", "The eBPF programs, and the glue that keeps the maps honest."],
    ["The joins", "How all of it fits together: leases, ownership, recovery, who cleans up after whom."],
  ];
  ours.forEach((o, i) => {
    const y = 2.2 + i * 1.12;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.12, w: 0.5, h: 0.5, fill: { color: MUSTARD } });
    text(s, o[0], { x: 1.5, y, w: 2.3, h: 0.75, valign: "middle", fontFace: H, fontSize: 21, bold: true, color: MUSTARD });
    text(s, o[1], { x: 3.9, y, w: 8.7, h: 0.85, valign: "middle", fontSize: 17, color: CREAM });
  });
  text(s, "Write the part that's yours. Borrow the rest.", { x: 0.7, y: 6.5, w: 11.9, h: 0.55, fontSize: 24, bold: true, italic: true, color: MUSTARD });
  s.addNotes(`[about 1 minute 45 seconds]

So what did we actually write?

Mustard, the gossip layer, is ours, and that's the one that needed justifying. Rule four says use the library, so what happened?

Rust doesn't have an equivalent of HashiCorp's memberlist, the Go library that everyone uses for this. There's a crate called foca, and it's decent. We evaluated it, and the design doc still carries the decision.

Three things we needed that didn't fit: messages that stay a fixed size no matter how big the cluster gets, our own piggyback payload riding along on every ping, and HMAC on every datagram. All three are the core of how this scales to ten thousand nodes, and all three mean reaching inside the protocol rather than sitting on top of it.

SWIM is also a small protocol. The paper is clear, and the core is about a thousand lines. So we wrote it: about six thousand lines now, with the tests.

That's the exception that proves rule four. You write it yourself when the thing you need is the part the library abstracts away. Not because it looks fun. It always looks fun.

Meat, the scheduler, is ours, because placement is the one thing nobody can do for you. It's your policy.

Onion's eBPF programs are ours: about 390 lines of C, plus the glue that keeps the kernel maps honest.

And then the biggest one, which doesn't have a name: the joins. How all of it fits together. Who owns what, what happens when a lease expires, who cleans up after a node dies halfway through something. That's where nearly all the bugs lived, and it's where I spent nearly all of my review time.

Write the part that's yours. Borrow the rest. That's not an AI lesson, it's just engineering, but the machines make it much easier to forget.`);
}

// ---------------------------------------------------------------- 23 Cost
{
  const s = lightSlide();
  kicker(s, "The receipt");
  title(s, "What did it cost?");
  const tiles = [
    ["200h", "of my time", KETCHUP],
    ["£810", "Claude, 9 months × £90", INK],
    ["£270", "Codex, 3 months × £90", INK],
    ["£10", "the domain", INK],
  ];
  tiles.forEach((t, i) => {
    const x = 0.7 + i * 3.05;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y: 2.4, w: 2.8, h: 2.4, rectRadius: 0.15, fill: { color: TINT } });
    text(s, t[0], { x, y: 2.6, w: 2.8, h: 1.2, align: "center", valign: "middle", fontFace: H, fontSize: 50, bold: true, color: t[2] });
    text(s, t[1], { x: x + 0.2, y: 3.9, w: 2.4, h: 0.7, align: "center", fontSize: 16, color: MUTED });
  });
  text(s, [
    { text: "£1,090 ", options: { bold: true, color: KETCHUP } },
    { text: "and 200 hours, for ~220k lines of Rust.", options: { color: INK } },
  ], { x: 0.7, y: 5.4, w: 11.9, h: 0.7, fontSize: 24 });
  s.addNotes(`[about 1 minute]

So what did it cost?

About 200 hours of my time. Nine months of Claude at ninety quid a month, that's 810. Three months of Codex, another 270. And ten quid for the domain, which is the only part I'm really proud of.

That comes to about a thousand pounds and 200 hours for roughly 220 thousand lines of Rust, in about 1,200 commits.

Three years ago that would have been a team of five for two years. I honestly don't know what to make of that yet.

[CHECK: line and commit counts are from the repo as of 21 Sep 2026. Recount on the day with: find src -name '*.rs' | xargs cat | wc -l ; git rev-list --count HEAD]`);
}

// ---------------------------------------------------------------- 24 Section: the fixes
section(4, "So what did I build?", "Every scar, and what Reliaburger does about it");

// ---------------------------------------------------------------- 25 One binary
{
  const s = lightSlide();
  kicker(s, "Fixes scar #2");
  title(s, "One binary. Batteries included.");
  const parts = [
    ["Bun", "the node agent"], ["Relish", "the CLI and TUI"], ["Mustard", "gossip"], ["Meat", "scheduler"],
    ["Onion", "eBPF service discovery"], ["Wrapper", "ingress"], ["Sesame", "PKI and mTLS"], ["Pickle", "image registry"],
    ["Mayo", "metrics"], ["Ketchup", "logs"], ["Lettuce", "GitOps"], ["Brioche", "web dashboard"],
  ];
  parts.forEach((p, i) => {
    const c = i % 4, r = Math.floor(i / 4);
    const x = 0.7 + c * 3.05, y = 2.3 + r * 1.25;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 2.8, h: 1.05, rectRadius: 0.12, fill: { color: i < 2 ? MUSTARD : TINT } });
    text(s, p[0], { x: x + 0.25, y: y + 0.15, w: 2.4, h: 0.45, fontFace: H, fontSize: 20, bold: true, color: INK });
    text(s, p[1], { x: x + 0.25, y: y + 0.58, w: 2.4, h: 0.4, fontSize: 15, color: MUTED });
  });
  text(s, "Yes, everything is named after a burger part. No, I'm not sorry.", { x: 0.7, y: 6.3, w: 11.9, h: 0.5, fontSize: 18, italic: true, color: KETCHUP });
  s.addNotes(`[about 1 minute]

So what did I build?

Remember the nine things from the start? Here they are again, compiled into one binary. The agent's called bun and the CLI's called relish. Gossip, scheduler, service discovery, ingress, PKI, a registry, metrics, logs, GitOps and a dashboard, all in the box.

Nothing to install next to it, and no compatibility matrix, because every version ships together and gets tested together.

And yes, everything is named after a part of a burger. No, I'm not sorry.`);
}

// ---------------------------------------------------------------- 26 Apps not pods
{
  const s = lightSlide();
  kicker(s, "Fixes scar #1");
  title(s, "Apps, not pods");
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y: 2.2, w: 6.3, h: 4.6, rectRadius: 0.12, fill: { color: BUN } });
  text(s, `[app.web]
image    = "myapp:v1.4.2"
replicas = 3
port     = 8080
memory   = "128Mi-512Mi"

[app.web.health]
path = "/healthz"

[app.web.ingress]
host = "myapp.com"

[app.web.autoscale]
min = 2
max = 10`, { x: 1.0, y: 2.4, w: 5.8, h: 4.3, fontFace: MONO, fontSize: 16, color: CREAM });
  text(s, "That's a Deployment, a Service, an Ingress, a certificate and an HPA.", { x: 7.5, y: 2.3, w: 5.1, h: 1.6, fontFace: H, fontSize: 24, bold: true, color: INK });
  text(s, [
    { text: "7 resource types in total", options: { bullet: true, breakLine: true } },
    { text: "replicas = \"*\" replaces DaemonSets", options: { bullet: true, breakLine: true } },
    { text: "TLS on by default", options: { bullet: true } },
  ], { x: 7.5, y: 4.2, w: 5.1, h: 2, fontSize: 19, color: MUTED, paraSpaceAfter: 10 });
  s.addNotes(`[about 1 minute]

Scar one was concepts. Here's a web app in Reliaburger. That's the whole thing.

It's TOML, not YAML. Three replicas, a memory range, a health check, a domain name and autoscaling. In Kubernetes that's a Deployment, a Service, an Ingress, a certificate and an HPA, at a minimum.

There are seven resource types in total. If you want something on every node, you say replicas equals star. There's no separate DaemonSet concept to learn.

And TLS on the ingress is on by default. You don't have to ask for it.`);
}

// ---------------------------------------------------------------- G1 Three layers
{
  const s = lightSlide();
  kicker(s, "Fixes scar #3 · Mustard");
  title(s, "Three layers, three jobs");
  const layers = [
    ["Reporting", "What's actually running where", "Every node → leader, every 5s, over mTLS", LETTUCE],
    ["Raft council", "Desired state. Who's the leader.", "3–7 nodes · 150ms heartbeat · 1–2s election timeout", KETCHUP],
    ["Mustard gossip", "Who's alive. Who leads.", "All nodes · UDP · 500ms rounds · small, fixed-size messages", MUSTARD],
  ];
  layers.forEach((l, i) => {
    const y = 2.3 + i * 1.45;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y, w: 7.6, h: 1.25, rectRadius: 0.15, fill: { color: TINT } });
    s.addShape(pres.shapes.OVAL, { x: 0.95, y: y + 0.3, w: 0.65, h: 0.65, fill: { color: l[3] } });
    text(s, l[0], { x: 1.85, y: y + 0.15, w: 2.6, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: INK });
    text(s, l[1], { x: 1.85, y: y + 0.65, w: 2.6, h: 0.5, fontSize: 15, color: MUTED });
    text(s, l[2], { x: 4.5, y: y + 0.2, w: 3.6, h: 0.9, valign: "middle", fontSize: 15, color: INK });
  });
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 8.7, y: 2.3, w: 3.9, h: 4.15, rectRadius: 0.15, fill: { color: BUN } });
  text(s, "Why split it?", { x: 9.0, y: 2.55, w: 3.4, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: MUSTARD });
  text(s, [
    { text: "Raft doesn't scale past single digits.", options: { breakLine: true } },
    { text: "Gossip scales to tens of thousands.", options: { breakLine: true } },
    { text: "So each one only does what it's good at.", options: {} },
  ], { x: 9.0, y: 3.15, w: 3.4, h: 3.1, fontSize: 17, color: CREAM, paraSpaceAfter: 12 });
  s.addNotes(`[about 1 minute 15 seconds]

Right, let's geek out a bit. Two parts of Reliaburger I'm proudest of: how the nodes find each other, and how the packets get where they're going.

Scar three was the pet control plane. In Kubernetes, every kubelet talks to the API server, and the API server talks to etcd. etcd is Raft, and Raft is brilliant, but it doesn't scale past single digits of voters. So you build a special, precious control plane around it.

Reliaburger splits the problem into three layers, and each one only does the thing it's good at.

At the bottom, Mustard. That's gossip, and every node takes part. It answers two questions: who's alive, and who's the leader. It's UDP, a round every 500 milliseconds, and the messages are small and bounded. Gossip scales to tens of thousands of nodes.

In the middle, the council: three to seven nodes running Raft. It holds the desired state, meaning what you asked for, and elects the leader. Heartbeats every 150 milliseconds, and elections time out after one to two seconds.

On top, reporting: what's actually running where. Every node reports to the leader every five seconds, over mTLS.

The key thing: Raft never has to talk to all the nodes. Gossip never has to carry big payloads.`);
}

// ---------------------------------------------------------------- G2 SWIM
{
  const s = lightSlide();
  kicker(s, "SWIM, in one slide");
  title(s, "How do you know a node is dead?");
  const flow = [
    ["PING", "Every 500ms, pick a random node", MUSTARD, INK],
    ["No ACK in 200ms?", "Ask 3 other nodes to PING it for you", MUSTARD, INK],
    ["Still nothing?", "Mark it SUSPECT and gossip that", "F8DFA0", INK],
    ["5 seconds to object", "The node can refute: “I'm alive”, with a higher incarnation", "F8DFA0", INK],
    ["DEAD", "The leader reschedules its apps", KETCHUP, WHITE],
  ];
  flow.forEach((f, i) => {
    const y = 2.2 + i * 0.9;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y, w: 3.2, h: 0.72, rectRadius: 0.12, fill: { color: f[2] } });
    text(s, f[0], { x: 0.7, y, w: 3.2, h: 0.72, align: "center", valign: "middle", fontFace: H, fontSize: 17, bold: true, color: f[3] });
    text(s, f[1], { x: 4.15, y, w: 4.3, h: 0.72, valign: "middle", fontSize: 16, color: INK });
  });
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 8.8, y: 2.2, w: 3.8, h: 4.3, rectRadius: 0.15, fill: { color: TINT } });
  text(s, "The clever bits", { x: 9.1, y: 2.45, w: 3.3, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: KETCHUP });
  text(s, [
    { text: "No broadcasts. News rides along on PINGs and ACKs, up to 8 updates each.", options: { bullet: true, breakLine: true } },
    { text: "Everyone hears everything in O(log N) rounds.", options: { bullet: true, breakLine: true } },
    { text: "In a secure cluster, every message is signed with HMAC-SHA256.", options: { bullet: true } },
  ], { x: 9.1, y: 3.05, w: 3.3, h: 3.3, fontSize: 15, color: INK, paraSpaceAfter: 10 });
  s.addNotes(`[about 1 minute 30 seconds]

So how do you know a node is dead in a cluster of thousands? You can't have everyone ping everyone. That's N squared, and it melts your network.

Reliaburger uses SWIM, a protocol from a 2002 paper out of Cornell. Every 500 milliseconds, each node picks one random peer and pings it.

No reply in 200 milliseconds? Maybe the network between us is flaky, so I ask three other nodes to ping it on my behalf. That's the indirect probe.

Still nothing? Now it's a suspect, and I tell everyone that. The suspect gets five seconds to object. If it hears the rumour about itself, it bumps its incarnation number and says "I'm alive, and here's proof it's newer than your rumour." That's what stops one packet loss from killing a healthy node.

No objection? Dead. The leader reschedules its apps.

And here's the bit I love: there are no broadcast messages. All the news, joins, deaths, a new leader, rides along on the pings and acks, up to eight updates per message. Like gossip at an office party, everybody knows everything in a logarithmic number of rounds.

In a secure cluster every message is signed with HMAC, keyed from the cluster secret, so you can't just spray fake "node X is dead" packets at the cluster.`);
}

// ---------------------------------------------------------------- G3 Council
{
  const s = darkSlide();
  kicker(s, "The council", true);
  text(s, "The control plane is elected, not installed", { x: 0.7, y: 1.0, w: 11.9, h: 0.9, fontFace: H, fontSize: 36, bold: true, color: CREAM });
  const cards = [
    ["Earned", "Picked from ordinary nodes. New zones first, then the oldest, most stable nodes."],
    ["Announced", "Raft picks the leader. Gossip tells all the other nodes who won."],
    ["Humble", "A new leader listens first: waits for 95% of reports, or 15 seconds."],
    ["Recoverable", "Lose a majority: it regrows. Lose all of it: relish council recover."],
  ];
  cards.forEach((c, i) => {
    const col = i % 2, row = Math.floor(i / 2);
    const x = 0.7 + col * 6.05, y = 2.3 + row * 2.05;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 5.85, h: 1.8, rectRadius: 0.15, fill: { color: "3A2A1F" } });
    text(s, c[0], { x: x + 0.35, y: y + 0.25, w: 5.2, h: 0.5, fontFace: H, fontSize: 22, bold: true, color: MUSTARD });
    text(s, c[1], { x: x + 0.35, y: y + 0.8, w: 5.2, h: 0.9, fontSize: 17, color: CREAM });
  });
  text(s, "Through all of it, your apps keep serving traffic.", { x: 0.7, y: 6.55, w: 11.9, h: 0.5, fontSize: 20, italic: true, color: "C9B8A6" });
  s.addNotes(`[about 1 minute 30 seconds]

Every node runs the same binary. There are no control-plane machines. The council gets elected from ordinary nodes.

You have to earn it. If you've labelled your zones, the council spreads across them first, because a council that lives in one rack isn't much of a council. After that, older nodes win, so the box you booted five minutes ago doesn't get handed the keys ahead of one that's been up for a month.

When Raft elects a new leader, the news goes out over gossip, so all the other nodes find out without Raft having to talk to them.

Then the new leader does something I wish more new managers did: it listens first. It waits until 95% of nodes have reported what they're actually running, or 15 seconds, before it makes any scheduling decisions. So it never acts on half a picture.

Lose a majority of the council, and it grows back from healthy nodes. Lose all of it, and you run relish council recover on a survivor, which restores from the last sealed backup.

I'll be honest about two things. Full-council recovery needs a human on purpose, because it throws away history, and a person picking which backup to restore is cheaper than an automated tie-break getting it wrong. And today every node reports straight to the leader. The two-level reporting tree for really big clusters is written, but not wired in yet.

Through all of it, your apps keep serving traffic. The data plane doesn't care who the leader is.`);
}

// ---------------------------------------------------------------- N1 No overlay
{
  const s = lightSlide();
  kicker(s, "Fixes scar #4 · Onion");
  title(s, "No overlay. No CNI. No kube-proxy.");
  const left = [
    ["Own network namespace", "for every container"],
    ["Real host ports", "from 10000–60000 on each node"],
    ["No tunnels", "no MTU surprises, no cluster IP space to run out of"],
  ];
  left.forEach((l, i) => {
    const y = 2.4 + i * 1.3;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.08, w: 0.55, h: 0.55, fill: { color: LETTUCE } });
    text(s, l[0], { x: 1.55, y, w: 5.6, h: 0.5, fontFace: H, fontSize: 22, bold: true, color: INK });
    text(s, l[1], { x: 1.55, y: y + 0.5, w: 5.6, h: 0.5, fontSize: 17, color: MUTED });
  });
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 7.6, y: 2.3, w: 5.0, h: 3.9, rectRadius: 0.15, fill: { color: TINT } });
  text(s, "So how does web find redis?", { x: 7.95, y: 2.6, w: 4.3, h: 1.0, fontFace: H, fontSize: 24, bold: true, color: KETCHUP });
  text(s, "With a name, a fake IP, and about 390 lines of C running inside the kernel.", { x: 7.95, y: 3.7, w: 4.3, h: 2.2, fontSize: 20, color: INK });
  s.addNotes(`[about 1 minute]

Now the networking. Scar four was "why can't A talk to B", and most of that pain comes from the layers. Overlay networks, CNI plugins, kube-proxy and thousands of iptables rules.

Reliaburger doesn't have an overlay. Every container gets its own network namespace, and it gets real ports on the host, from a range of fifty thousand per node. No tunnels, no encapsulation, no MTU surprises, no cluster-wide IP space to run out of. If you tcpdump on the host, you see real addresses.

So how does the web app find redis, when redis could be on any node, on any port, and move around?

With a name, a fake IP address, and about 390 lines of C running inside the kernel.`);
}

// ---------------------------------------------------------------- N2 Connection flow
{
  const s = lightSlide();
  kicker(s, "One connection, step by step");
  title(s, "What happens when web calls redis");
  const steps = [
    ["1", "getaddrinfo(\"redis.payments.internal\")", "Bun's DNS responder answers from the service map", MUSTARD],
    ["2", "→ 127.128.0.42", "A virtual IP per service, in the loopback range, so it can't leak onto the network", MUSTARD],
    ["3", "connect(127.128.0.42, 6379)", "An eBPF hook on the cgroup catches the syscall", KETCHUP],
    ["4", "→ 10.0.1.5:30891", "The kernel rewrites it to a healthy backend. Direct TCP.", KETCHUP],
  ];
  steps.forEach((st, i) => {
    const y = 2.2 + i * 1.1;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.12, w: 0.6, h: 0.6, fill: { color: st[3] } });
    text(s, st[0], { x: 0.7, y: y + 0.12, w: 0.6, h: 0.6, align: "center", valign: "middle", fontFace: H, fontSize: 20, bold: true, color: WHITE });
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 1.55, y, w: 5.9, h: 0.85, rectRadius: 0.1, fill: { color: BUN } });
    text(s, st[1], { x: 1.8, y, w: 5.5, h: 0.85, valign: "middle", fontFace: MONO, fontSize: 16, bold: true, color: CREAM });
    text(s, st[2], { x: 7.75, y, w: 4.85, h: 0.85, valign: "middle", fontSize: 16, color: INK });
  });
  text(s, "No proxy in the data path. If Bun crashes, connections keep flowing.", { x: 0.7, y: 6.6, w: 11.9, h: 0.5, fontSize: 20, italic: true, bold: true, color: KETCHUP });
  s.addNotes(`[about 1 minute 30 seconds]

Let's follow one connection.

The web app wants redis in the payments namespace. It calls getaddrinfo on redis.payments.internal, like any app would. Bun runs a small DNS responder, and it answers from the service map with a virtual IP. Every service gets one, from 127.128 slash 16. That's inside the loopback range on purpose: if anything ever goes wrong, those packets physically can't leave the machine.

Then the app calls connect() on that virtual IP. This is where the magic happens. There's an eBPF program attached to the cgroup, on the connect hook. It catches the syscall before any packet exists, looks up the virtual IP in a map, picks a healthy backend, and rewrites the destination to the real host and port. Say 10.0.1.5, port 30891.

The app gets a plain, direct TCP connection to the backend. It has no idea any of this happened.

There's no proxy in the data path. No sidecar, no kube-proxy, no userspace hop per packet. It's a hash lookup at connect time and then the kernel gets out of the way.

And because the program lives in the kernel, if Bun crashes, existing connections carry on, and new ones still go to the backends the map already knows about.`);
}

// ---------------------------------------------------------------- N3 Inside the hook
{
  const s = lightSlide();
  kicker(s, "Inside the hook");
  title(s, "One hook, three jobs");
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 0.7, y: 2.2, w: 6.9, h: 4.6, rectRadius: 0.12, fill: { color: BUN } });
  text(s, `SEC("cgroup/connect4")
...
if ((dst_ip & VIP_MASK) != VIP_PREFIX)
    return egress_allowed(...);

fault = lookup(&fault_connect_map, ...);
if (fault && roll < fault->probability)
    return 0;          /* EPERM */

val = lookup(&backend_map, &key);
if (src_ns != val->namespace_id &&
    !allowed(&firewall_map, ...))
    return 0;          /* EPERM */

/* round-robin a healthy backend */
ctx->user_ip4  = backend->host_ip;
ctx->user_port = backend->host_port;`, { x: 0.95, y: 2.4, w: 6.5, h: 4.3, fontFace: MONO, fontSize: 13, color: CREAM });
  const jobs = [
    ["Discovery", "Virtual IP → healthy backend", LETTUCE],
    ["Firewall", "Namespace isolation, allow_from, egress allowlists", KETCHUP],
    ["Chaos", "Smoker drops or partitions connections, right here", MUSTARD],
  ];
  jobs.forEach((j, i) => {
    const y = 2.2 + i * 1.6;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 7.95, y, w: 4.65, h: 1.35, rectRadius: 0.15, fill: { color: TINT } });
    s.addShape(pres.shapes.OVAL, { x: 8.2, y: y + 0.4, w: 0.55, h: 0.55, fill: { color: j[2] } });
    text(s, j[0], { x: 9.0, y: y + 0.2, w: 3.4, h: 0.5, fontFace: H, fontSize: 20, bold: true, color: INK });
    text(s, j[1], { x: 9.0, y: y + 0.7, w: 3.4, h: 0.6, fontSize: 14, color: MUTED });
  });
  s.addNotes(`[about 1 minute 15 seconds]

Here's the actual hook, trimmed down. It's C, compiled to eBPF, and Bun loads it from Rust using a library called aya.

Not a virtual IP? Then it's regular egress, and we check the egress allowlist if the app has one.

Then a fault check. That's Smoker, the chaos engine. It can drop a percentage of connections using the kernel's random number generator, or partition two apps completely. Same hook, a few lines.

Then the backend lookup. If the caller is in a different namespace, the firewall map has to say yes, or the app gets EPERM. That's namespace isolation, with no NetworkPolicy and no CNI that may or may not support it.

Then round-robin a healthy backend and rewrite the address.

So one hook, at one point in the kernel, does service discovery, the firewall and fault injection. In Kubernetes those are three different projects.`);
}

// ---------------------------------------------------------------- N4 The one we got wrong
{
  const s = darkSlide();
  kicker(s, "The one we got wrong", true);
  text(s, "The plan: do DNS in the kernel too", { x: 0.7, y: 1.0, w: 11.9, h: 0.9, fontFace: H, fontSize: 36, bold: true, color: CREAM });
  const steps = [
    ["The design", "Catch DNS queries in a cgroup hook and answer them straight from a BPF map."],
    ["The catch", "Those hooks can rewrite where a packet goes. They can't read what's inside it."],
    ["The detour", "A proof of concept at the TC layer worked. It also needed a second DNS parser, checksums, TCP and IPv6 fallbacks…"],
    ["The call", "A small DNS responder in userspace. Keep connect() in the kernel, where the speed matters."],
  ];
  steps.forEach((st, i) => {
    const y = 2.2 + i * 1.08;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y: y + 0.1, w: 0.5, h: 0.5, fill: { color: i === 3 ? LETTUCE : MUSTARD } });
    text(s, st[0], { x: 1.5, y, w: 2.4, h: 0.7, valign: "middle", fontFace: H, fontSize: 20, bold: true, color: i === 3 ? "9CCB63" : MUSTARD });
    text(s, st[1], { x: 3.9, y, w: 8.7, h: 0.9, valign: "middle", fontSize: 17, color: CREAM });
  });
  text(s, "Boring won. Boring usually does.", { x: 0.7, y: 6.55, w: 11.9, h: 0.5, fontSize: 22, italic: true, bold: true, color: MUSTARD });
  s.addNotes(`[about 1 minute]

And here's one we got wrong, because every good war story needs one.

The original design did DNS in the kernel as well. Catch the DNS query in a cgroup hook, look the name up in a BPF map and answer straight from the kernel. Beautiful on paper. It's still in the design doc, marked as abandoned.

The catch: those cgroup hooks can change where a packet goes, but they can't read what's inside it. So you can't tell which name was asked for, never mind build a reply.

We took a detour. A proof of concept one layer down, at TC, actually worked. It could synthesise a DNS answer. It also needed a second DNS parser, checksums, and fallbacks for TCP, IPv6 and fragments. That's a lot of clever code in a place where bugs are really hard to debug.

So we made the boring call. A small DNS responder in userspace, and keep the connect() rewrite in the kernel, which is where the speed actually matters.

Boring won. Boring usually does. Rule four from earlier, in action.`);
}

// ---------------------------------------------------------------- 30 More batteries
{
  const s = lightSlide();
  kicker(s, "Things you'd normally install separately");
  title(s, "And the rest of the box");
  const feats = [
    ["Registry", "Push images to the cluster. P2P distribution."],
    ["Metrics", "Built-in time series. Query with SQL."],
    ["Logs", "Collected and indexed. Nothing to deploy."],
    ["Dashboards", "Web UI and a terminal UI."],
    ["GitOps", "Point it at a repo. Done."],
    ["Chaos", "Smoker: fault injection, built in."],
    ["Self-upgrade", "Rolling binary upgrades. Workloads survive."],
    ["Processes", "Run plain binaries, not just containers."],
    ["Security", "mTLS by default. SPIFFE identity. Secrets in git."],
    ["relish wtf", "What's wrong with my cluster? Across every node."],
    ["relish trace", "Why can't A talk to B? Follows the real path."],
    ["K8s import", "relish import turns your YAML into TOML, with a report."],
  ];
  feats.forEach((f, i) => {
    const c = i % 4, r = Math.floor(i / 4);
    const x = 0.7 + c * 3.05, y = 2.2 + r * 1.55;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 2.8, h: 1.38, rectRadius: 0.15, fill: { color: TINT } });
    text(s, f[0], { x: x + 0.25, y: y + 0.18, w: 2.4, h: 0.45, fontFace: H, fontSize: 18, bold: true, color: f[0].startsWith("relish") ? KETCHUP : INK });
    text(s, f[1], { x: x + 0.25, y: y + 0.63, w: 2.4, h: 0.7, fontSize: 13, color: MUTED });
  });
  s.addNotes(`[about 1 minute 30 seconds]

Quickly, the rest of the box.

A built-in image registry: push to the cluster, and images spread peer to peer. Metrics you can query with SQL, because everyone knows SQL. Logs collected and indexed without deploying anything. A web dashboard and a terminal UI. GitOps: point it at a repo.

And Smoker, a built-in fault injector. I wrote a book on chaos engineering, so there was no way this thing was shipping without one.

It upgrades itself: rolling binary replacement, and your workloads keep running through it. And it can run plain processes, not just containers.

Security's on by default: mTLS between nodes, a SPIFFE identity for every workload, and secrets encrypted so you can keep them in git.

And the two commands I've wanted for ten years. relish wtf goes to every node and tells you what's wrong with your cluster. relish trace follows the real path from A to B and tells you where it breaks. You'll see wtf in the demo.

If you're coming from Kubernetes, relish import turns your YAML into TOML and tells you what it couldn't translate. relish export goes the other way, so it's never a dead end.`);
}

// ---------------------------------------------------------------- 32 Demo
{
  const s = darkSlide();
  s.addShape(pres.shapes.OVAL, { x: 5.17, y: 1.2, w: 3.0, h: 3.0, fill: { color: KETCHUP } });
  text(s, "DEMO", { x: 5.17, y: 1.2, w: 3.0, h: 3.0, align: "center", valign: "middle", fontFace: H, fontSize: 48, bold: true, color: WHITE });
  text(s, "Let's see if the demo gods are with us", { x: 0.7, y: 4.8, w: 11.9, h: 0.7, align: "center", fontSize: 26, italic: true, color: MUSTARD });
  s.addNotes(`[5 minutes]

Right, enough slides. Let's see if the demo gods are with us.

Suggested flow (5 min):
1. bun --runtime process  (no container runtime needed)
2. relish apply examples/phase-1/proc-first-run.toml
3. relish status, then relish (the TUI), then the web dashboard on :9117
4. Break something: relish chaos / relish fault, or kill an instance, and watch it come back
5. relish wtf
6. If time allows: ./scripts/kubernetes-yamls-demo.sh for import/export

Have a recording ready as a fallback. This is SREday: everybody in the room will appreciate a demo that fails, but they'll appreciate a backup plan more.`);
}

// ---------------------------------------------------------------- 33 What it isn't
{
  const s = lightSlide();
  kicker(s, "Honesty slide");
  title(s, "What it deliberately doesn't do");
  const no = ["CRDs", "Operators", "Helm", "Service mesh", "Distributed volumes", "Overlay networks", "Windows", "IPv6 (yet)"];
  no.forEach((n, i) => {
    const c = i % 4, r = Math.floor(i / 4);
    const x = 0.7 + c * 3.05, y = 2.3 + r * 1.2;
    s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x, y, w: 2.8, h: 0.95, rectRadius: 0.12, fill: { color: TINT } });
    text(s, n, { x, y, w: 2.8, h: 0.95, align: "center", valign: "middle", fontFace: H, fontSize: 17, bold: true, color: MUTED, strike: "sngStrike" });
  });
  text(s, "If you need those, stay on Kubernetes. Seriously.", { x: 0.7, y: 5.2, w: 11.9, h: 0.7, fontSize: 28, bold: true, italic: true, color: KETCHUP });
  s.addNotes(`[about 1 minute]

Now the honest bit. Here's what Reliaburger deliberately doesn't do.

No CRDs and no operators. No Helm, because when your config is fifteen lines of TOML you don't need a templating engine for it. No service mesh. No distributed volumes: volumes are local, and for data that must survive a node dying, use a managed database or something that replicates itself. No overlay network. No Windows, and no IPv6 yet.

If you need those things, stay on Kubernetes. Seriously. It's very good at them.

Reliaburger is for the other teams. The ones with a handful to a few hundred nodes, running web apps and APIs, who want containers in production without a platform team.

And it's 0.1.0. It's going to have rough edges. Which brings me to you.`);
}

// ---------------------------------------------------------------- 34 Join us
{
  const s = darkSlide();
  kicker(s, "Join us on this journey", true);
  text(s, "This is a weird, unique moment", { x: 0.7, y: 1.0, w: 11.9, h: 0.9, fontFace: H, fontSize: 44, bold: true, color: CREAM });
  text(s, [
    { text: "One person with a thousand quid can build an orchestrator.", options: { breakLine: true } },
    { text: "That window might not stay open.", options: { breakLine: true } },
    { text: "Either the machines take over, or it turns out the models were heavily subsidised.", options: {} },
  ], { x: 0.7, y: 2.4, w: 7.3, h: 3, fontSize: 24, color: "C9B8A6", paraSpaceAfter: 16 });
  text(s, "Let's make something good with it.", { x: 0.7, y: 5.6, w: 7.5, h: 0.7, fontSize: 28, bold: true, italic: true, color: MUSTARD });
  memeBox(s, 8.5, 1.9, 4.1, 4.2, true);
  s.addNotes(`[about 1 minute]

So here's where I'd like to end up.

We're in a weird, unique moment. One person, working evenings, with about a thousand quid, can build a real container orchestrator. That's never been true before.

I'm not sure how long it stays true. Either the machines take over and none of us are needed, or it turns out the models were heavily subsidised and the real price is ten times higher. Probably something in between. But right now, the window's open.

So I'd like you to join me on this journey. Let's make something good with it while we can.

[MEME idea: Gandalf "All we have to decide is what to do with the time that is given us", or the "Now is the time" meme.]`);
}

// ---------------------------------------------------------------- 35 How to help
{
  const s = lightSlide();
  kicker(s, "What you can do");
  title(s, "Download it. Break it. Tell me.");
  const steps = [
    ["1", "Download and have a play", "curl -fsSL https://reliaburger.com/install.sh | bash"],
    ["2", "Break it", "It's 0.1.0. It will break. That's the point."],
    ["3", "Send us GitHub issues", "github.com/reliaburger/reliaburger"],
  ];
  steps.forEach((st, i) => {
    const y = 2.3 + i * 1.35;
    s.addShape(pres.shapes.OVAL, { x: 0.7, y, w: 0.8, h: 0.8, fill: { color: [LETTUCE, MUSTARD, KETCHUP][i] } });
    text(s, st[0], { x: 0.7, y, w: 0.8, h: 0.8, align: "center", valign: "middle", fontFace: H, fontSize: 26, bold: true, color: WHITE });
    text(s, st[1], { x: 1.8, y: y - 0.05, w: 8, h: 0.5, fontFace: H, fontSize: 24, bold: true, color: INK });
    text(s, st[2], { x: 1.8, y: y + 0.45, w: 8, h: 0.45, fontFace: i === 1 ? B : MONO, fontSize: i === 1 ? 18 : 16, color: MUTED });
  });
  s.addShape(pres.shapes.ROUNDED_RECTANGLE, { x: 9.9, y: 2.3, w: 2.7, h: 2.7, rectRadius: 0.15, fill: { color: TINT }, line: { color: "D9CDBD", width: 1.5, dashType: "dash" } });
  text(s, "QR code to the repo", { x: 9.9, y: 3.4, w: 2.7, h: 0.5, align: "center", fontSize: 13, italic: true, color: "B3A594" });
  text(s, "Free. Apache 2.0. No relicensing, ever. The book's in the repo too.", { x: 0.7, y: 6.3, w: 11.9, h: 0.5, fontSize: 18, italic: true, color: KETCHUP });
  s.addNotes(`[about 1 minute]

So what can you do to help?

Download it and have a play. It's one curl command, or build it from source. You don't even need a container runtime for the first try.

Break it. It's 0.1.0. It will break, and that's the point. You're SREs; breaking things is basically your job description.

Then send us GitHub issues. Every bug you find now is one that doesn't page someone later.

It's free and Apache 2.0, and it's staying that way. No relicensing, no bait and switch. And the book, both Everything Wrong with Kubernetes and the one on how Reliaburger got built, lives in the repo too.

[Add a QR code for the repo in the dashed box.]`);
}

// ---------------------------------------------------------------- 36 Thanks
{
  const s = darkSlide();
  s.addImage({ path: LOGO, x: 0.7, y: 1.3, w: 4.8, h: 4.8, rounding: true });
  text(s, "Thank you!", { x: 6.2, y: 1.9, w: 6.4, h: 1.2, fontFace: H, fontSize: 60, bold: true, color: CREAM });
  text(s, "Miko Pawlikowski", { x: 6.2, y: 3.3, w: 6.4, h: 0.6, fontSize: 26, color: MUSTARD });
  text(s, [
    { text: "reliaburger.com", options: { breakLine: true } },
    { text: "github.com/reliaburger/reliaburger", options: {} },
  ], { x: 6.2, y: 4.2, w: 6.4, h: 1.2, fontFace: MONO, fontSize: 18, color: "C9B8A6", paraSpaceAfter: 8 });
  s.addNotes(`[end at about 36:00 on paper, so about 30 when you speed up]

Thank you! I'm Miko. The repo and the site are on screen. Come and find me afterwards, I'd love to hear what's wrong with it.

[Final MEME slot if you want one: a burger with the caption "one binary".]`);
}

pres.writeFile({ fileName: path.join(__dirname, "reliaburger-sreday.pptx") }).then(f => console.log("wrote", f));
