export const highlightFixtures = [
  {
    id: "sentence", title: "A replaced phrase", extension: "md",
    description: "The spaces inside the replaced phrase join its highlight; the surrounding sentence stays unchanged.",
    before: "The installer uses old broken settings until setup finishes.",
    after: "The installer uses new valid configuration until setup finishes.",
    beforeChanges: ["old broken settings"], afterChanges: ["new valid configuration"]
  },
  {
    id: "separate-phrases", title: "Two separate replacements", extension: "md",
    description: "Each replaced phrase is continuous, but the unchanged words between the phrases remain unhighlighted.",
    before: "Choose the slow local build and keep the old debug logs.",
    after: "Choose the fast remote image and keep the new trace files.",
    beforeChanges: ["slow local build", "old debug logs"], afterChanges: ["fast remote image", "new trace files"]
  },
  {
    id: "edges", title: "Indentation and trailing spaces", extension: "txt",
    description: "The identical leading indentation and trailing spaces keep their original background. Only the internal space is added to the highlight.",
    before: "    old worker    ", after: "    new runner    ",
    beforeChanges: ["old worker"], afterChanges: ["new runner"]
  },
  {
    id: "tabs", title: "Multiple spaces and an internal tab", extension: "txt",
    description: "A complete horizontal whitespace run joins two changed words without changing any of its characters or spacing.",
    before: "Prefix old  \t slow worker suffix.", after: "Prefix new  \t fast runner suffix.",
    beforeChanges: ["old  \t slow worker"], afterChanges: ["new  \t fast runner"]
  },
  {
    id: "unchanged-word", title: "An unchanged word breaks the highlight", extension: "md",
    description: "Spaces beside an unchanged word are not absorbed. The word 'and' still separates the two edits.",
    before: "Use old and slow code.", after: "Use new and fast code.",
    beforeChanges: ["old", "slow"], afterChanges: ["new", "fast"]
  },
  {
    id: "punctuation", title: "Unchanged punctuation is not swallowed", extension: "md",
    description: "The unchanged comma and its following space still separate these word changes.",
    before: "Use old, slow code.", after: "Use new, fast code.",
    beforeChanges: ["old", "slow"], afterChanges: ["new", "fast"]
  },
  {
    id: "syntax", title: "Code with syntax coloring", extension: "js",
    description: "The phrase inside the string gets one continuous highlight. Quotes, code syntax, indentation and trailing spaces stay as they were.",
    before: "    const label = \"old slow worker\";  ", after: "    const label = \"new fast runner\";  ",
    beforeChanges: ["old slow worker"], afterChanges: ["new fast runner"]
  },
  {
    id: "single-word", title: "A single-word edit does not grow", extension: "txt",
    description: "A space is not highlighted just because one side changed. This case deliberately looks identical before and after.",
    before: "    Keep the old value.    ", after: "    Keep the new value.    ",
    beforeChanges: ["old"], afterChanges: ["new"]
  },
  {
    id: "whitespace-edit", title: "Real whitespace edits remain visible", extension: "txt",
    description: "Intentional indentation and trailing-space changes keep their existing highlights. The new rule only adds internal gap highlighting.",
    before: "    keep value   ", after: "\tkeep value ",
    beforeChanges: ["    ", "   "], afterChanges: ["\t", " "]
  },
  {
    id: "nonbreaking-space", title: "An internal non-breaking space", extension: "txt",
    description: "Non-breaking spaces between changed words also join the highlight; text contents and wrapping rules are preserved.",
    before: "Prefix old\u00a0slow worker suffix.", after: "Prefix new\u00a0fast runner suffix.",
    beforeChanges: ["old\u00a0slow worker"], afterChanges: ["new\u00a0fast runner"]
  }
];

export const wrappedFixture = {
  id: "wrapped", title: "Installer paragraph, wrapped on mobile", extension: "md",
  description: "A longer sample installer paragraph, with the real widget's normal mobile wrapping and line-number gutters.",
  before: "The installer uses old broken settings until setup finishes and then starts the background service with the selected project directory. Missing configs deliberately defer service setup.",
  after: "The installer uses new valid configuration until setup finishes and then launches the background worker with the selected project directory. Missing configs deliberately defer service setup."
};

export const reportedFixture = {
  id: "reported-example", title: "The installer edit from your screenshot", extension: "md",
  description: "The four edited source lines reconstructed from your screenshot. Matching words still break a highlight; matching spaces between changed text no longer do.",
  before: [
    "startup-invalid configs deliberately defer service setup: quickstart creates or",
    "repairs the config and then offers to install and start the service. Set",
    "`CODEXIFY_SKIP_SERVICE=1` in the installer process to skip service handling even",
    "when a valid config exists."
  ].join("\n"),
  after: [
    "startup-invalid configs deliberately defer service setup until the selected",
    "configuration is valid; the installer's next step directs the user to quickstart.",
    "Set `CODEXIFY_SKIP_SERVICE=1` in the installer process to skip service handling",
    "even when a valid config exists."
  ].join("\n")
};

export function diffPayload(fixture) {
  const path = `examples/${fixture.id}.${fixture.extension}`;
  const before = fixture.before.split("\n"), after = fixture.after.split("\n");
  return {
    summary: { files: 1, additions: after.length, deletions: before.length },
    files: [{ path, status: "modified", additions: after.length, deletions: before.length }],
    patchIncluded: true,
    patch: [
      `diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`,
      `@@ -138,${before.length} +138,${after.length} @@`,
      ...before.map(line => "-" + line), ...after.map(line => "+" + line), ""
    ].join("\n")
  };
}
