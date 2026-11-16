@axis_extensibility @claim_batteries_included @smoke
Feature: External operational integration
  As an operator whose requirements exceed built-in capabilities
  I want to connect external systems through supported interfaces
  So that the integrated platform does not become an adoption dead end

  @req_EXT-001 @claim_apps_export_traces_to_external_collectors @claim_no_otlp_opentelemetry_export
  Scenario: Export application telemetry to an external collector
    Given a healthy disposable Reliaburger cluster
    And a reachable external telemetry collector in the test lab
    And an instrumented fixture application
    When I configure telemetry export using documented supported interfaces
    And I generate 100 uniquely identifiable application requests
    Then the external collector should receive telemetry attributable to the fixture
    And the evidence should classify the integration as native, configured, workaround, source_change, or unsupported
    And the evidence should record all required product source modifications
