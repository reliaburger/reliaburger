"""The released-CLI snapshot reads a published relish's own help, faithfully.

`released-cli.tsv` is what tests/suite/website.rs checks the site, the README
and the tour against, so a parser that drops an option or a value would let an
unreleased command through, or flag a released one.
"""
import unittest

from released_cli import parse_help, snapshot, version_of

ROOT_HELP = """Reliaburger CLI

Usage: relish [OPTIONS] [COMMAND]

Commands:
  apply              Apply a Reliaburger TOML or Kubernetes YAML manifest
  manual             Read the built-in manual (searchable TUI; --web for the browser)
  help               Print this message or the help of the given subcommand(s)

Options:
      --output <OUTPUT>      Output format: human, json, or yaml [default: human] [possible values: human, json, yaml]
      --token <TOKEN>        API token for authenticating to the agent. Overrides `RELIABURGER_TOKEN`
  -h, --help                 Print help
  -V, --version              Print version
"""

APPLY_HELP = """Apply a Reliaburger TOML or Kubernetes YAML manifest

Usage: relish apply [OPTIONS] <PATH_OR_URL|--file <PATH_OR_URL>>

Arguments:
  [PATH_OR_URL]  Manifest path or https:// URL

Options:
  -f, --file <PATH_OR_URL>   Manifest path or https:// URL (the kubectl spelling)
      --output <OUTPUT>      Output format: human, json, or yaml [default: human] [possible values: human, json, yaml]
      --dry-run              Show the plan without deploying (exits 0 even with no agent)
  -h, --help                 Print help (see more with '--help')
"""

MANUAL_HELP = """Read the built-in manual

Usage: relish manual [OPTIONS] [CHAPTER] [COMMAND]

Commands:
  examples  Write the embedded example configs into a directory
  help      Print this message or the help of the given subcommand(s)

Arguments:
  [CHAPTER]  Open this chapter, e.g. `tour` or `chaos`

Options:
      --web                  Serve the manual as one HTML page and open the browser
  -h, --help                 Print help
"""

EXAMPLES_HELP = """Write the embedded example configs into a directory

Usage: relish manual examples [OPTIONS] [DIR]...

Arguments:
  [DIR]...  Where to write them

Options:
  -h, --help  Print help
"""

HELP = {
    (): ROOT_HELP,
    ("apply",): APPLY_HELP,
    ("manual",): MANUAL_HELP,
    ("manual", "examples"): EXAMPLES_HELP,
}


class ParseHelp(unittest.TestCase):
    def test_commands_leave_out_clap_help(self):
        commands, _, _ = parse_help(ROOT_HELP)
        self.assertEqual(commands, ["apply", "manual"])

    def test_options_keep_every_alias_their_value_and_its_choices(self):
        _, _, options = parse_help(APPLY_HELP)
        self.assertIn((["-f", "--file"], "<PATH_OR_URL>", []), options)
        self.assertIn((["--output"], "<OUTPUT>", ["human", "json", "yaml"]), options)
        self.assertIn((["--dry-run"], "", []), options)

    def test_positionals_keep_their_shape(self):
        _, arguments, _ = parse_help(EXAMPLES_HELP)
        self.assertEqual(arguments, ["[DIR]..."])


class Snapshot(unittest.TestCase):
    def test_walks_every_subcommand_and_writes_sorted_rows(self):
        rows = snapshot("v0.1.6", lambda path: HELP[tuple(path)])
        self.assertEqual(rows[0], "version\tv0.1.6")
        self.assertIn("command\trelish manual examples", rows)
        self.assertIn("argument\trelish manual\t[CHAPTER]", rows)
        self.assertIn("option\trelish apply\t-f,--file\t<PATH_OR_URL>\t", rows)
        self.assertIn("option\trelish\t--output\t<OUTPUT>\thuman,json,yaml", rows)
        self.assertNotIn("option\trelish apply\t--output\t<OUTPUT>\thuman,json,yaml", rows)
        self.assertIn("command\trelish help", rows)
        self.assertNotIn("command\trelish manual help", rows)
        self.assertEqual(rows[1:], sorted(rows[1:]))

    def test_version_comes_from_the_binary(self):
        self.assertEqual(version_of("relish 0.1.6 (82fa78e)\n"), "v0.1.6")
        with self.assertRaises(ValueError):
            version_of("relish 0.2.0-dev (1ea24fd)\n")


if __name__ == "__main__":
    unittest.main()
