@axis_operations @claim_integrated_platform
Feature: Network perimeter
  As an operator running nodes on a shared network
  I want cluster ports closed to everyone but members and operators
  So that the control plane is not exposed to the rest of the network

  @req_OPS-004 @claim_network_perimeter_firewall
  Scenario Outline: Port <port> as seen from <source>
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    And a probe source address for <source>
    When I open a TCP connection to <port> on every node from that address
    Then every node should answer <expected>

    Examples:
      | case               | source          | port               | expected |
      | operator-api       | the operator    | the API port 9117  | open     |
      | operator-gossip    | the operator    | gossip port 9443   | filtered |
      | operator-raft      | the operator    | Raft port 9444     | filtered |
      | operator-reporting | the operator    | reporting port 9445| filtered |
      | outsider-api       | a non-member    | the API port 9117  | filtered |
      | outsider-raft      | a non-member    | Raft port 9444     | filtered |
      | outsider-ingress   | a non-member    | ingress port 80    | open     |
