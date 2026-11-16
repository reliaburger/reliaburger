@axis_extensibility @claim_batteries_included
Feature: Machine-readable CLI output
  As an operator automating the cluster from scripts
  I want every read command to honour --output json
  So that automation does not have to scrape human tables

  @req_EXT-002 @claim_cli_machine_readable_output
  Scenario Outline: relish <command> honours --output json
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    When I run relish <command> with --output json from the workstation
    Then the command should exit 0
    And its standard output should parse as JSON

    Examples: commands the claim names
      | case      | command        |
      | status    | status         |
      | nodes     | nodes          |
      | council   | council        |
      | tokens    | token list     |
      | audit     | audit --since 1h |

    Examples: other read commands under the global --output flag
      | case      | command               |
      | inspect   | inspect frontend      |
      | routes    | routes                |
      | resolve   | resolve backend       |
      | faults    | fault list            |
      | history   | history frontend      |
      | jobs      | jobs                  |
      | images    | images                |
      | top       | top                   |
      | metrics   | metrics frontend      |
      | wtf       | wtf                   |
