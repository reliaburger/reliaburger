/**
 * Reliaburger feedback backend - Google Apps Script web app ("Reliaburger feedback").
 *
 * Serves https://reliaburger.com/feedback/ (docs/website/feedback/index.html). Workshop attendees answer Miko's
 * three questions (biggest Kubernetes problem, what Reliaburger needs for work use, the ONE missing feature) and
 * leave their name, email and LinkedIn, with an explicit "happy to share my information with the Reliaburger
 * team" consent. Four required 1-3 ratings (overall, easy to digest, fits my stack, would recommend) and up to
 * 10 screenshots, attached raw to the email as "<Name> - screenshot N.<ext>" (the page scales big ones down). The script:
 *   1. emails ONE message From mark@sreday.com, To the same address, Cc FEEDBACK_CC (Script Property, comma-separated,
 *      e.g. Miko), Reply-To the attendee. Subject "<Name> - Reliaburger feedback - <event>". Filed in the Inbox,
 *      unread + important, under the Gmail label "Reliaburger feedback".
 *   2. appends a row to a Google Sheet ("Reliaburger feedback", created on first use, id in FEEDBACK_SHEET_ID).
 *      Filter it by event to read all answers side by side.
 *      Rows are never deleted by code.
 *
 * Same conventions as the conference forms in llmday/_build/*.gs (waitlist-form.gs, communityhero-form.gs):
 * JSON posted as text/plain (no CORS preflight), honeypot field `website`, daily cap, length caps, LinkedIn host
 * check, createDraft().send() then file the thread.
 *
 * Deploy (one-time, from the Google account that sends as mark@sreday.com):
 *   1. https://script.google.com -> New project -> paste this file -> save as "Reliaburger feedback".
 *   2. Project settings -> Script properties -> add FEEDBACK_CC = <Miko's email> (optional, comma-separated).
 *   3. Run testFeedback once from the editor (authorises Gmail + Sheets, creates the sheet), check the log.
 *      It is a dry run: nothing is sent. Run testFeedbackSend to send one real email to yourself and add a row.
 *   4. Deploy -> New deployment -> type "Web app" -> Execute as: Me -> Who has access: Anyone. Copy the .../exec URL.
 *   5. Put that URL into docs/website/feedback/index.html -> FEEDBACK_URL, push (GitHub Pages deploys it).
 *   Re-deploy after editing: Deploy -> Manage deployments -> edit -> new version (the URL stays the same).
 */

var SENDER = 'mark@sreday.com';
var SENDER_NAME = 'Reliaburger feedback';
var LABEL = 'Reliaburger feedback';
var DAILY_MAX = 300;
var MAX_IMAGE_BYTES = 10 * 1024 * 1024;   // per screenshot, same cap as the page
var MAX_SHOTS = 10;
var MAX_TOTAL_BYTES = 20 * 1024 * 1024;   // all screenshots together; Gmail refuses messages over 25 MB
var RATINGS = [
  ['overall', 'Overall, I enjoyed the experience'],
  ['digest', 'It was easy to digest'],
  ['stack', 'It would fit in my stack once it\'s properly cooked'],
  ['recommend', 'I\'d recommend it to a teammate']
];                      // submissions per day (a full room is ~100)
var LIMITS = { platform_other: 80, name: 80, email: 254, linkedin: 300, company: 120, role: 120, event: 60, answer: 2000, broke: 2000 };
var PLATFORMS = ['macOS', 'Linux', 'Other'];
var STEPS = ['Install', 'Deploy an app', 'Sick version + rollback', 'Break it on purpose', 'Manual and source', 'Submitted a PR'];
var QUESTIONS = [
  ['q1', 'What is your BIGGEST problem with the Kubernetes stack?'],
  ['q2', 'What would Reliaburger need to do so that your team can use it at work?'],
  ['q3', 'What\'s the ONE missing feature?']
];

function doGet() {
  return respond({ ok: true, service: 'reliaburger-feedback', sheet: !!props().getProperty('FEEDBACK_SHEET_ID'),
                   today: readJson('FEEDBACK_DAILY').count || 0 });
}

function doPost(e) {
  var data;
  try {
    data = JSON.parse(e.postData.contents);
  } catch (err) {
    return respond({ ok: false, error: 'bad json' });
  }
  if (data.website) return respond({ ok: true });            // honeypot -> pretend success

  var s = normalize(data);
  if (s.errors.length) return respond({ ok: false, error: 'invalid', fields: s.errors });

  var attachments = [], shots = Array.isArray(data.shots) ? data.shots.slice(0, MAX_SHOTS) : [];
  for (var i = 0; i < shots.length; i++) {
    var sh = shots[i] || {};
    var blob = decodeImage(sh.b64, photoMime(sh.type, sh.name), s.name + ' - screenshot ' + (attachments.length + 1) + photoExt(sh.type, sh.name));
    if (blob === 'too large') return respond({ ok: false, error: 'image too large' });
    if (blob) attachments.push(blob);
  }
  var totalBytes = attachments.reduce(function (n, b) { return n + b.getBytes().length; }, 0);
  if (totalBytes > MAX_TOTAL_BYTES) return respond({ ok: false, error: 'images too large' });
  s.shots = attachments.length;

  var mail = compose(s);
  if (data.dry_run) return respond({ ok: true, dry_run: true, subject: mail.subject, text: mail.text, html: mail.html,
                                     attachments: attachments.map(function (b) { return b.getName(); }) });
  if (!dailyBudget()) return respond({ ok: false, error: 'too many today' });

  var options = { name: SENDER_NAME, replyTo: s.email, htmlBody: mail.html };
  if (attachments.length) options.attachments = attachments;
  var cc = clean(props().getProperty('FEEDBACK_CC'), 500);
  if (cc) options.cc = cc;
  // Send from the alias when this account has it as "Send mail as"; otherwise the primary address is used.
  var to = SENDER;
  if (GmailApp.getAliases().indexOf(SENDER) !== -1) options.from = SENDER;
  else to = Session.getEffectiveUser().getEmail();

  var message = GmailApp.createDraft(to, mail.subject, mail.text, options).send();
  fileThread(message);
  record(s);
  Logger.log('Feedback: %s <%s> at %s, %s screenshots', s.name, s.email, s.event, s.shots);
  return respond({ ok: true });
}

// ---- input -----------------------------------------------------------------------------

function normalize(d) {
  var s = {
    name:        clean(d.name, LIMITS.name),
    email:       clean(d.email, LIMITS.email).toLowerCase(),
    linkedin:    clean(d.linkedin, LIMITS.linkedin),
    consent:     d.consent === true,
    company:     clean(d.company, LIMITS.company),
    role:        clean(d.role, LIMITS.role),
    platform:    PLATFORMS.indexOf(d.platform) !== -1 ? d.platform : '',
    platform_other: d.platform === 'Other' ? clean(d.platform_other, LIMITS.platform_other) : '',
    steps:       pick(d.steps, STEPS),
    q1:          cleanMultiline(d.q1, LIMITS.answer),
    q2:          cleanMultiline(d.q2, LIMITS.answer),
    q3:          cleanMultiline(d.q3, LIMITS.answer),
    broke:       cleanMultiline(d.broke, LIMITS.broke),
    contributor: d.contributor === true,
    event:       clean(d.event, LIMITS.event).toLowerCase().replace(/[^a-z0-9-]/g, '') || 'general',
    page:        clean(d.page, 300),
    ratings:     {},
    errors:      []
  };
  if (!s.name) s.errors.push('name');
  if (!/^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/.test(s.email)) s.errors.push('email');
  if (!/^https?:\/\/([a-z]{2,3}\.)?(www\.)?linkedin\.com\/.+/i.test(s.linkedin)) s.errors.push('linkedin');
  if (!s.consent) s.errors.push('consent');
  ['broke', 'company', 'role', 'platform'].forEach(function (k) { if (!s[k]) s.errors.push(k); });
  if (s.platform === 'Other' && !s.platform_other) s.errors.push('platform_other');
  if (!s.steps.length) s.errors.push('steps');
  var r = d.ratings || {};
  RATINGS.forEach(function (q) {
    var v = parseInt(r[q[0]], 10);
    if (v >= 1 && v <= 3) s.ratings[q[0]] = v; else s.errors.push('rating_' + q[0]);
  });
  QUESTIONS.forEach(function (q) { if (!s[q[0]]) s.errors.push(q[0]); });
  return s;
}

// ---- the email -------------------------------------------------------------------------

function compose(s) {
  var subject = s.name + ' - Reliaburger feedback - ' + s.event;
  var who = [
    ['Name', s.name], ['Email', s.email], ['LinkedIn', s.linkedin],
    ['Happy to share their information with the Reliaburger team', 'yes']
  ];
  var answers = QUESTIONS.map(function (q) { return [q[1], s[q[0]]]; });
  answers.push(['What broke? (if anything)', s.broke]);
  var rates = RATINGS.map(function (q) { return [q[1], s.ratings[q[0]] + ' / 3']; });
  var more = [
    ['Company', s.company], ['Role', s.role], ['Platform', platformText(s)],
    ['Steps completed', s.steps.length ? s.steps.join(', ') : ''],
    ['Screenshots attached', s.shots ? String(s.shots) : ''],
    ['Wants to contribute / be a burger ambassador', s.contributor ? 'yes' : 'no'],
    ['Event', s.event]
  ].filter(function (r) { return r[1]; });

  var text = [who, answers, rates, more].map(function (rows) {
    return rows.map(function (r) { return r[0] + (/\?$/.test(r[0]) ? '' : ':') + '\n' + r[1]; }).join('\n\n');
  }).join('\n\n----------\n\n') + '\n';

  function table(rows) {
    return '<table cellpadding="6" style="border-collapse:collapse;margin:0 0 18px">' + rows.map(function (r) {
      var v = r[0] === 'LinkedIn' ? '<a href="' + esc(r[1]) + '">' + esc(r[1]) + '</a>'
            : r[0] === 'Email' ? '<a href="mailto:' + esc(r[1]) + '">' + esc(r[1]) + '</a>'
            : esc(r[1]).replace(/\n/g, '<br>');
      return '<tr><td style="vertical-align:top;font-weight:bold;width:230px;border-bottom:1px solid #ddd">' + esc(r[0]) +
             '</td><td style="vertical-align:top;border-bottom:1px solid #ddd">' + v + '</td></tr>';
    }).join('') + '</table>';
  }
  var html = '<div style="font-family:Arial,Helvetica,sans-serif;font-size:14px;line-height:1.5;color:#111">' +
    '<h3 style="color:#a83b15;margin:0 0 12px">Reliaburger feedback - ' + esc(s.event) + '</h3>' +
    table(who) + table(answers) + table(rates) + (more.length ? table(more) : '') + '</div>';
  return { subject: subject, text: text, html: html };
}

function platformText(s) {
  return s.platform === 'Other' && s.platform_other ? 'Other: ' + s.platform_other : s.platform;
}

// ---- the sheet -------------------------------------------------------------------------
// One spreadsheet, created on first use and remembered in FEEDBACK_SHEET_ID. Never cleared by code.

var COLUMNS = ['timestamp', 'event', 'name', 'email', 'linkedin', 'consent', 'company', 'role', 'platform', 'steps',
               'q1_biggest_k8s_problem', 'q2_needed_for_work', 'q3_one_missing_feature', 'what_broke', 'contributor', 'page',
               // added later: kept at the end so rows written before them stay aligned
               'rating_overall', 'rating_easy_to_digest', 'rating_fits_my_stack', 'rating_would_recommend', 'screenshots'];

function feedbackSheet() {
  var id = props().getProperty('FEEDBACK_SHEET_ID'), ss = null;
  if (id) { try { ss = SpreadsheetApp.openById(id); } catch (err) { ss = null; } }
  if (!ss) {
    ss = SpreadsheetApp.create('Reliaburger feedback');
    ss.getSheets()[0].appendRow(COLUMNS);
    ss.getSheets()[0].setFrozenRows(1);
    props().setProperty('FEEDBACK_SHEET_ID', ss.getId());
    Logger.log('Created the feedback sheet: ' + ss.getUrl());
  }
  var sheet = ss.getSheets()[0];
  // A sheet created by an older version has a shorter header: extend it in place.
  if (sheet.getLastColumn() < COLUMNS.length) sheet.getRange(1, 1, 1, COLUMNS.length).setValues([COLUMNS]);
  return sheet;
}

function record(s) {
  try {
    feedbackSheet().appendRow([new Date().toISOString(), s.event, s.name, s.email, s.linkedin, s.consent ? 'yes' : 'no',
                               s.company, s.role, platformText(s), s.steps.join(', '), s.q1, s.q2, s.q3, s.broke,
                               s.contributor ? 'yes' : 'no', s.page,
                               s.ratings.overall, s.ratings.digest, s.ratings.stack, s.ratings.recommend, s.shots || 0].map(sheetSafe));
  } catch (err) {
    Logger.log('Emailed, but could not add the sheet row: ' + err);    // the email is the backup
  }
}

// A cell starting with = + - @ would be read as a formula; prefix it so it stays text.
function sheetSafe(v) {
  return typeof v === 'string' && /^[=+\-@]/.test(v) ? '\'' + v : v;
}

// ---- filing, budget, helpers (same conventions as llmday/_build/waitlist-form.gs) -------

function fileThread(message) {
  try {
    var thread = message.getThread();
    thread.moveToInbox();
    thread.markUnread();
    thread.markImportant();
    var label = GmailApp.getUserLabelByName(LABEL) || GmailApp.createLabel(LABEL);
    thread.addLabel(label);
  } catch (err) {
    Logger.log('Sent, but could not file the thread: ' + err);
  }
}

function props() { return PropertiesService.getScriptProperties(); }

function dailyBudget() {
  var today = Utilities.formatDate(new Date(), 'UTC', 'yyyy-MM-dd');
  var d = readJson('FEEDBACK_DAILY');
  if (d.day !== today) d = { day: today, count: 0 };
  if (d.count + 1 > DAILY_MAX) return false;
  d.count += 1;
  props().setProperty('FEEDBACK_DAILY', JSON.stringify(d));
  return true;
}

function readJson(key) {
  try { return JSON.parse(props().getProperty(key) || '{}') || {}; } catch (e) { return {}; }
}
function respond(obj) {
  return ContentService.createTextOutput(JSON.stringify(obj)).setMimeType(ContentService.MimeType.JSON);
}
function clean(v, max) {
  return String(v == null ? '' : v).replace(/[\x00-\x1f\x7f]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, max);
}
// keeps line breaks, strips other control chars, normalises CRLF
function cleanMultiline(v, max) {
  return String(v == null ? '' : v).replace(/\r\n?/g, '\n').replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]+/g, ' ')
    .replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim().slice(0, max);
}
function esc(v) {
  return String(v).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
var IMAGE_TYPES = { 'image/jpeg': '.jpg', 'image/png': '.png', 'image/webp': '.webp', 'image/heic': '.heic', 'image/heif': '.heif', 'image/gif': '.gif' };

function photoExt(type, name) {
  type = String(type || '').toLowerCase();
  if (IMAGE_TYPES[type]) return IMAGE_TYPES[type];
  var m = /\.([a-z0-9]{2,5})$/i.exec(String(name || ''));
  return m ? '.' + m[1].toLowerCase().replace(/^jpeg$/, 'jpg') : '.png';
}
function photoMime(type, name) {
  type = String(type || '').toLowerCase();
  if (IMAGE_TYPES[type]) return type;
  var ext = photoExt(type, name);
  for (var k in IMAGE_TYPES) if (IMAGE_TYPES[k] === ext) return k;
  return 'application/octet-stream';
}
// base64 -> Blob; returns null when empty or undecodable, 'too large' when over the cap
function decodeImage(b64, mime, filename) {
  b64 = String(b64 || '').replace(/^data:[^,]*,/, '').replace(/\s+/g, '');
  if (!b64) return null;
  if (b64.length * 3 / 4 > MAX_IMAGE_BYTES) return 'too large';
  try {
    return Utilities.newBlob(Utilities.base64Decode(b64), mime, filename);
  } catch (err) {
    return null;
  }
}

// Keep only allowed values, in the allowed list's order, without duplicates.
function pick(values, allowed) {
  if (!Array.isArray(values)) return [];
  return allowed.filter(function (a) { return values.indexOf(a) !== -1; });
}

// ---- editor tests ----------------------------------------------------------------------

function sample(dryRun) {
  return { postData: { contents: JSON.stringify({
    dry_run: dryRun, name: 'Test Burger', email: SENDER, linkedin: 'https://www.linkedin.com/in/marek-pawlikowski/',
    consent: true, company: 'SREday', role: 'Organizer', platform: 'Other', platform_other: 'Fedora on a ThinkPad', steps: ['Install', 'Deploy an app', 'Break it on purpose', 'Submitted a PR'],
    q1: 'Too many moving parts.\nUpgrades eat a week every quarter.', q2: 'A stable 1.0 and a migration path from Helm.',
    q3: 'Windows support', broke: 'relish wtf printed nothing the first time.', contributor: true,
    ratings: { overall: 3, digest: 2, stack: 3, recommend: 3 },
    shots: [{ b64: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==', type: 'image/png', name: 'wtf.png' }],
    event: 'sreday-sf-2026-q4', page: 'https://reliaburger.com/feedback/?event=sreday-sf-2026-q4'
  }) } };
}

// Dry run: composes and logs the email, sends nothing, creates the sheet if missing.
function testFeedback() {
  Logger.log(doPost(sample(true)).getContent());
  Logger.log('Sheet: ' + feedbackSheet().getParent().getUrl());
}

// Real run: sends one email to SENDER (+ FEEDBACK_CC) and appends one row.
function testFeedbackSend() {
  Logger.log(doPost(sample(false)).getContent());
}
