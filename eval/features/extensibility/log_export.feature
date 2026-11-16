@axis_extensibility @claim_batteries_included
Feature: Log export for external analysis
  As an operator with a data or SIEM pipeline
  I want to take the cluster's logs out in a standard format
  So that other tools can read them

  @req_EXT-007 @claim_log_and_metric_export_to_object_storage
  Scenario: Exported Parquet logs are readable by a standard tool
    Given a healthy disposable Reliaburger cluster
    And an app printing a unique log marker
    When I export the logs of the node running it to the workstation with relish logs-export
    Then a standard Parquet reader on the workstation should find the marker
    And relish logs-search on the workstation should find the same marker
