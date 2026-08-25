##############################################################################
# The runner box -- a Firecracker host.
#
# One dedicated m8i.large. Each hosted project runs in its own microVM with its
# own guest kernel, so tenants are separated by hardware virtualisation rather
# than by a shared kernel's permission checks. This is the same isolation Lambda
# and Fargate use, because it is the same hypervisor.
#
# This was not affordable before February 2026. Firecracker needs KVM, AWS only
# exposed KVM on bare metal, and the cheapest x86 metal is c5.metal at about
# $2,978/month. AWS then added a NestedVirtualization CPU option to ordinary
# instances -- no additional charge, and per the EC2 user guide there is no
# minimum instance size, only a list of supported types. That is what makes the
# line below the load-bearing one in this file, and the whole design possible at
# $97/month rather than $2,978.
#
#   Verified against the AWS EC2 User Guide, August 2026:
#     - supported: C8i M8i R8i C8id R8id M8id *-flex X8i C7i R7i M7i I7i
#     - "There is no additional cost for using nested virtualization."
#     - KVM is a supported L1 hypervisor
#     - Graviton is NOT supported, so the cheaper ARM families are out
#
# Requires AWS provider >= 6.0; nested_virtualization does not exist in 5.x.
##############################################################################

variable "runner_subnet_id" {
  description = "Subnet for the runner. Same VPC as boxcode-auth so the two can talk privately."
  type        = string
}

variable "runner_vpc_id" {
  type = string
}

variable "runner_instance_type" {
  description = <<-EOT
    m8i.large: 2 vCPU / 8 GiB, $0.10584/hr in us-east-1. Sized by memory, not by
    CPU -- a microVM's memory is spent from the host the moment it boots, since
    it has its own kernel and nothing is shared or reclaimable. Ten at 256 MiB
    plus the host side is about 3.7 GiB, which is 45% of this box.

    Must stay on a nested-virtualization-capable Intel type. m7i.large is about
    $3.68/month cheaper and also supported, but is an older core; the February
    announcement credited 8th-gen microarchitecture for the nested performance.
  EOT
  type        = string
  default     = "m8i.large"
}

variable "runner_root_gb" {
  description = "Guest kernels, per-project rootfs images, Postgres, retained zips and logs."
  type        = number
  default     = 50
}

variable "bootstrap_bucket" {
  description = "Bucket holding the infra/hosting bundle the box fetches to provision itself. One key, read-only -- see the instance role."
  type        = string
  default     = "boxcode-artifacts"
}

variable "admin_cidr" {
  description = "Source allowed to reach SSH. Empty closes port 22, which is right once Session Manager is proven."
  type        = list(string)
  default     = []
}

locals {
  runner_tags = {
    Name               = "boxcode-runner"
    (var.cost_tag_key) = "true"
  }
}

data "aws_ssm_parameter" "al2023" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

# CloudFront's own egress ranges, maintained by AWS. Using this rather than
# 0.0.0.0/0 means resolving apps.boxcode.sh and hitting the origin directly gets
# nothing, so WAF and the rate limits cannot be skipped by avoiding the edge.
data "aws_ec2_managed_prefix_list" "cloudfront" {
  name = "com.amazonaws.global.cloudfront.origin-facing"
}

##############################################################################
# Network
##############################################################################

resource "aws_security_group" "runner" {
  name        = "boxcode-runner"
  description = "boxcode Firecracker host. Answers CloudFront only."
  vpc_id      = var.runner_vpc_id
  tags        = local.runner_tags
}

resource "aws_vpc_security_group_ingress_rule" "runner_https" {
  security_group_id = aws_security_group.runner.id
  prefix_list_id    = data.aws_ec2_managed_prefix_list.cloudfront.id
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  description       = "HTTPS from CloudFront only"
}

# Not for traffic -- CloudFront speaks HTTPS to the origin. Open because Let's
# Encrypt validates the HTTP-01 challenge from its own addresses, not CloudFront's.
resource "aws_vpc_security_group_ingress_rule" "runner_acme" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "tcp"
  from_port         = 80
  to_port           = 80
  description       = "HTTP for the Lets Encrypt HTTP-01 challenge"
}

resource "aws_vpc_security_group_ingress_rule" "runner_ssh" {
  for_each          = toset(var.admin_cidr)
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = each.value
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  description       = "SSH from an administrator"
}

# The HOST needs egress: packages, Let's Encrypt, Session Manager, and pulling
# dependencies while building a project's rootfs. GUESTS do not get it, and that
# containment is not expressible here -- each microVM sits on a point-to-point
# /30 TAP link inside its own network namespace, and the host does not forward
# for it. See runtime/network.mjs.
resource "aws_vpc_security_group_egress_rule" "runner_out" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Host egress. Guests are confined on the box, not here"
}

##############################################################################
# Instance role -- as close to empty as it can be
##############################################################################

data "aws_iam_policy_document" "runner_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "runner" {
  name               = "boxcode-runner"
  assume_role_policy = data.aws_iam_policy_document.runner_assume.json
  tags               = local.runner_tags
}

# The only managed policy attached, and the only reason this box holds
# credentials at all. It buys two things: SSH can stay closed, and the kill
# switch can reach the box during an incident without depending on a key.
resource "aws_iam_role_policy_attachment" "runner_ssm" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# Its own log group and nothing else. Note what is absent and must stay absent:
# no s3:*, no ec2:*, no secretsmanager:*, no iam:*, no lambda:*. If a tenant ever
# escapes both its microVM and the jailer and reads these credentials off the
# metadata service, there is nothing here worth having.
data "aws_iam_policy_document" "runner" {
  # One object, on one prefix. The box has to fetch infra/hosting/ from
  # somewhere to provision itself, and this is the narrowest way to let it: not
  # the bucket, not a prefix it could walk, one key it can GET. Everything the
  # bundle contains is already in the repo, so this grants read access to
  # nothing that is not public in intent.
  statement {
    sid       = "FetchItsOwnProvisioningBundle"
    actions   = ["s3:GetObject"]
    resources = ["arn:aws:s3:::${var.bootstrap_bucket}/hosting/hosting.tgz"]
  }

  statement {
    sid       = "OwnLogsOnly"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/boxcode/runner:*"]
  }
}

resource "aws_iam_role_policy" "runner" {
  role   = aws_iam_role.runner.id
  policy = data.aws_iam_policy_document.runner.json
}

resource "aws_iam_instance_profile" "runner" {
  name = "boxcode-runner"
  role = aws_iam_role.runner.name
}

##############################################################################
# The instance
##############################################################################

resource "aws_instance" "runner" {
  ami                    = data.aws_ssm_parameter.al2023.value
  instance_type          = var.runner_instance_type
  subnet_id              = var.runner_subnet_id
  vpc_security_group_ids = [aws_security_group.runner.id]
  iam_instance_profile   = aws_iam_instance_profile.runner.name

  # The line this entire design depends on. Without it /dev/kvm does not exist
  # and Firecracker cannot start a single microVM -- setup.sh checks for exactly
  # that and refuses early rather than letting it surface as a confusing failure
  # on the first deploy.
  cpu_options {
    nested_virtualization = "enabled"
  }

  root_block_device {
    volume_size = var.runner_root_gb
    volume_type = "gp3"
    encrypted   = true
  }

  metadata_options {
    http_tokens = "required" # IMDSv2 only
    # A guest has no route to the metadata service at all, but the jailed VMM
    # processes run on the host. Hop limit 1 reaches the host and nothing behind it.
    http_put_response_hop_limit = 1
  }

  # Nested virtualization can only be changed while stopped, and the AMI moves
  # under us as Amazon publishes new ones. Neither should trigger a replacement
  # of a box holding live projects.
  lifecycle {
    ignore_changes = [ami]
  }

  tags = local.runner_tags
}

variable "allocate_eip" {
  description = <<-EOT
    Whether to give the runner a stable Elastic IP.

    Required in production: apps.boxcode.sh points at it, and a certificate
    issued for a name that stops resolving here breaks every project. Not
    required to test, where the instance's auto-assigned address is enough and
    SKIP_TLS=1 skips the certificate entirely.

    Off by default because this account is at its Elastic IP limit, and the two
    unassociated addresses sitting in it belong to other people's work.
    Releasing an Elastic IP is irreversible -- the address does not come back --
    so that is a decision for whoever owns them, not a step in this apply.
  EOT
  type        = bool
  default     = false
}

resource "aws_eip" "runner" {
  count    = var.allocate_eip ? 1 : 0
  instance = aws_instance.runner.id
  domain   = "vpc"
  tags     = local.runner_tags
}

##############################################################################
# Backups
##############################################################################

resource "aws_iam_role" "dlm" {
  name = "boxcode-runner-dlm"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "dlm.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
  tags = local.runner_tags
}

resource "aws_iam_role_policy_attachment" "dlm" {
  role       = aws_iam_role.dlm.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSDataLifecycleManagerServiceRole"
}

resource "aws_dlm_lifecycle_policy" "runner" {
  description        = "Daily snapshot of the boxcode runner root volume"
  execution_role_arn = aws_iam_role.dlm.arn
  state              = "ENABLED"

  policy_details {
    resource_types = ["VOLUME"]
    # By tag rather than by id, so a volume replaced during maintenance keeps
    # being backed up without anyone remembering to re-point this.
    target_tags = { (var.cost_tag_key) = "true" }

    schedule {
      name = "daily"
      create_rule {
        interval      = 24
        interval_unit = "HOURS"
        times         = ["04:00"]
      }
      # Seven days at roughly 30 GB retained is the $1.50/month in the estimate.
      retain_rule {
        count = 7
      }
      copy_tags = true
    }
  }

  tags = local.runner_tags
}

##############################################################################
# Alarms
##############################################################################

# The box is gone or wedged, so every project is down. Nothing else will say so.
resource "aws_cloudwatch_metric_alarm" "runner_status" {
  alarm_name          = "boxcode-runner-status-check"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "StatusCheckFailed"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  dimensions          = { InstanceId = aws_instance.runner.id }
  # An instance that stopped reporting is at least as concerning as one
  # reporting a failure.
  treat_missing_data = "breaching"
  alarm_description  = "The Firecracker host is failing status checks. Every hosted project is down."
  alarm_actions      = [aws_sns_topic.alerts.arn]
  ok_actions         = [aws_sns_topic.alerts.arn]
  tags               = local.runner_tags
}

# A full disk takes out every microVM and Postgres together, is caused by
# ordinary use, and is the outage this box will actually have. Needs the
# CloudWatch agent to report; without it this sits in INSUFFICIENT_DATA, which
# is itself worth noticing.
resource "aws_cloudwatch_metric_alarm" "runner_disk" {
  alarm_name          = "boxcode-runner-disk"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "disk_used_percent"
  namespace           = "CWAgent"
  period              = 300
  statistic           = "Maximum"
  threshold           = 80
  alarm_description   = "Runner disk above 80%. A full disk takes every project down together."
  alarm_actions       = [aws_sns_topic.alerts.arn]
  tags                = local.runner_tags
}

output "runner_instance_id" {
  value = aws_instance.runner.id
}

output "runner_public_ip" {
  description = "Point apps.boxcode.sh at this, and set it as the CloudFront origin for /api/*. The Elastic IP when there is one, otherwise the instance's own address -- which changes on stop/start, so it is fine for testing and not for DNS."
  value       = var.allocate_eip ? one(aws_eip.runner[*].public_ip) : aws_instance.runner.public_ip
}
