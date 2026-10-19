@axis_extensibility @claim_batteries_included
Feature: Extension points
  As an operator whose needs outgrow the built-in features
  I want a supported way to add my own resource types and automation
  So that I can extend the platform instead of replacing it

  @req_EXT-005 @claim_no_crds_operators_plugins_custom_schedulers
  Scenario Outline: Extend the platform with <mechanism>
    Given a healthy disposable Reliaburger cluster
    When I try to extend the platform with <mechanism>
    Then the extension should work through a documented interface

    Examples:
      | case          | mechanism                                     |
      | custom-kind   | a custom resource kind in the app config      |
      | cli-plugin    | a relish plugin executable on PATH            |
      | admission     | an admission or validation webhook            |
