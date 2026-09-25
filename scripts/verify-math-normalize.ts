// Quick verification of `normalizeMathDelimiters` against real inputs.
// Loaded via `deno run --allow-read scripts/verify-math-normalize.ts`.
// The regex source is copied verbatim from chat.js so a failure here
// points at the regex logic itself, not at a port.

function normalizeMathDelimiters(text: string): string {
  const LATEX_CMD = /\\[a-zA-Z]+/;
  text = text.replace(
    /\[\s*\n([\s\S]*?)\n\s*\](?!\s*\()/g,
    (m: string, inner: string) => {
      if (!LATEX_CMD.test(inner)) return m;
      return `\\[\n${inner}\n\\]`;
    },
  );
  text = text.replace(
    /(?<!\\)\[(\s*\\[a-zA-Z][\s\S]*?)\](?!\s*\()/g,
    (m: string, inner: string) => `\\[${inner}\\]`,
  );
  return text;
}

interface Case {
  name: string;
  in: string;
  mustContain?: string[];
  mustNotContain?: string[];
}

const cases: Case[] = [
  {
    // The exact snippet from the user report: the LLM wraps each
    // display-math block in [ ... ] (its own LaTeX-flavoured
    // bracket pair). The normalizer must turn both into \[...\].
    name: "user's exact example — multi-line bracketed math",
    in:
      "[\n" +
      "\\text{Consommation en ampères} = \\frac{\\text{Puissance en watts}}{\\text{Tension en volts}}\n" +
      "]\n\n" +
      "Par exemple, pour un four de 3000 watts à 230 volts :\n" +
      "[\n" +
      "\\text{Consommation en ampères} = \\frac{3000}{230} \\approx 13,04 \\text{ A}\n" +
      "]",
    mustContain: [
      "\\[\n\\text{Consommation en ampères} = \\frac{\\text{Puissance en watts}}{\\text{Tension en volts}}\n\\]",
      "\\[\n\\text{Consommation en ampères} = \\frac{3000}{230} \\approx 13,04 \\text{ A}\n\\]",
      "Par exemple, pour un four de 3000 watts",
    ],
  },
  {
    name: "single-line bracketed math also rewrites",
    in: "Inline math: [ \\frac{a}{b} ] here.",
    mustContain: ["\\[ \\frac{a}{b} \\]"],
  },
  {
    name: "plain markdown link is untouched",
    in: "See [the docs](https://example.com).",
    mustNotContain: ["\\["],
  },
  {
    name: "bracketed prose without LaTeX is untouched",
    in: "Note: [this is just a note].",
    mustNotContain: ["\\["],
  },
  {
    name: "bracketed text with LaTeX but followed by (url) is left alone (would break the link)",
    in: "Click [\\frac{a}{b}](https://example.com) for details.",
    mustNotContain: ["\\[\\frac{a}{b}\\]("],
  },
  {
    name: "plain $$ block is untouched (already valid)",
    in: "$$x^2 + y^2 = z^2$$",
    mustNotContain: ["\\["],
  },
  {
    name: "real link followed by prose is preserved end-to-end",
    in: "Read [the spec](https://spec.org) for the math:\n[\\frac{a}{b}]\nthen continue.",
    mustContain: ["[the spec](https://spec.org)"],
    mustNotContain: ["\\[\\frac{a}{b}\\]("],
  },
];

let pass = 0;
let fail = 0;
for (const c of cases) {
  const out = normalizeMathDelimiters(c.in);
  let caseFail = false;
  for (const want of c.mustContain ?? []) {
    if (!out.includes(want)) {
      console.error(`FAIL ${c.name}: missing ${JSON.stringify(want)}`);
      console.error(`  input:  ${JSON.stringify(c.in)}`);
      console.error(`  output: ${JSON.stringify(out)}`);
      caseFail = true;
    }
  }
  for (const nope of c.mustNotContain ?? []) {
    if (out.includes(nope)) {
      console.error(`FAIL ${c.name}: should not contain ${JSON.stringify(nope)}`);
      console.error(`  input:  ${JSON.stringify(c.in)}`);
      console.error(`  output: ${JSON.stringify(out)}`);
      caseFail = true;
    }
  }
  if (caseFail) fail++;
  else {
    console.log(`PASS ${c.name}`);
    pass++;
  }
}
console.log(`\n${pass} passed, ${fail} failed`);
Deno.exit(fail > 0 ? 1 : 0);
