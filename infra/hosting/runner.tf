##############################################################################
# The runner box.
#
# One dedicated t3.medium, on-demand. No auto scaling group, no spot, no
# container runtime -- hosted apps are ordinary systemd services on this one
# instance. Provisioned by running infra/hosting/setup.sh on it.
#
# It holds about ten small backends. That number is not configured anywhere: the
# control-plane admits a deploy when the box has the memory for it and refuses
# when it does not, so ten tiny FastAPI services and six Next.js servers are
# both correct answers. See runtime/capacity.mjs.
#
# The instance role is deliberately close to empty. Someone who gets code
# execution here should find an instance that can do nothing to the rest of the
# account -- no S3, no other EC2, no secrets, no IAM. What little it has is
# Session Manager, so the kill switch can reach it and SSH can stay shut.
##############################################################################

variable "runner_subnet_id" {
  description = "Subnet for the runner. Same VPC as boxcode-auth so the two can talk privately."
  type        = string
}

variable "runner_vpc_id" {
  type = string
}

variable "runner_instance_type" {
  description = "t3.medium: 2 vCPU / 4 GiB, which holds ~10 small backends once nothing is spent on a container runtime."
  type        = string
  default     = "t3.medium"
}

variable "runner_root_gb" {
  description = "Apps, their built dependencies, Postgres, retained zips and the journal all live here."
  type        = number
  default     = 60
}

variable "admin_cidr" {
  description = "Source allowed to reach SSH. Leave empty to close port 22 entirely, which is the right answer once Session Manager is proven."
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
# nothing -- so WAF and the rate limits cannot be walked around by skipping the
# edge.
data "aws_ec2_managed_prefix_list" "cloudfront" {
  name = "com.amazonaws.global.cloudfront.origin-facing"
}

##############################################################################
# Network
##############################################################################

resource "aws_security_group" "runner" {
  name        = "boxcode-runner"
  description = "boxcode hosted backends. Answers CloudFront only."
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

# Not for traffic -- CloudFront speaks HTTPS to the origin. This is open because
# Let's Encrypt validates the HTTP-01 challenge from its own addresses, which
# are not CloudFront's.
resource "aws_vpc_security_group_ingress_rule" "runner_acme" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "tcp"
  from_port         = 80
  to_port           = 80
  description       = "HTTP for the Let's Encrypt HTTP-01 challenge"
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

# The HOST needs egress: package installs, Let's Encrypt, Session Manager, and
# `npm install` during a build. Hosted apps do NOT get it, and that containment
# is not done here -- it is IPAddressDeny=any in each app's systemd unit, which
# is a per-service BPF filter. A security group cannot say "this process may
# reach the internet and that one may not" on one host, so it does not try.
resource "aws_vpc_security_group_egress_rule" "runner_out" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Host egress. Apps are confined by their systemd units, not here."
}

##############################################################################
# The instance role -- as close to empty as it can be
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

# The only managed policy attached, and the only reason the box has credentials
# at all. It buys two things worth having: SSH can stay closed, and the kill
# switch can reach the box during an incident without depending on a key.
resource "aws_iam_role_policy_attachment" "runner_ssm" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# Its own log group and nothing else. Note what is absent and should stay
# absent: no s3:*, no ec2:*, no secretsmanager:*, no iam:*, no lambda:*. If a
# hosted app ever escapes its unit and reads these credentials from the metadata
# service, there is nothing here worth having.
data "aws_iam_policy_document" "runner" {
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

  root_block_device {
    volume_size = var.runner_root_gb
    volume_type = "gp3"
    encrypted   = true
  }

  metadata_options {
    http_tokens = "required" # IMDSv2 only
    # An app that somehow gets a network exception still cannot read the
    # instance role: hop limit 1 reaches the host and nothing behind it.
    http_put_response_hop_limit = 1
  }

  # A demo host, not a pet: everything on it is either reproducible from
  # setup.sh or is 48-hour project state. Stop/start rather than replace is the
  # normal maintenance path, so the root volume must survive a stop.
  lifecycle {
    ignore_changes = [ami]
  }

  tags = local.runner_tags
}

# A stable address is not optional: apps.boxcode.sh points at it, and a cert
# issued for a name that stops resolving to this box breaks every hosted app.
resource "aws_eip" "runner" {
  instance = aws_instance.runner.id
  domain   = "vpc"
  tags     = local.runner_tags
}

##############################################################################
# Backups -- the only thing standing between a lost volume and lost projects
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
    # Matches the volume by tag rather than by id, so a volume replaced during
    # maintenance keeps being backed up without anyone remembering to re-point
    # this.
    target_tags = { (var.cost_tag_key) = "true" }

    schedule {
      name = "daily"
      create_rule {
        interval      = 24
        interval_unit = "HOURS"
        times         = ["04:00"]
      }
      retain_rule {
        count = 7
      }
      copy_tags = true
    }
  }

  tags = local.runner_tags
}

##############################################################################
# The two alarms worth having for a single box
##############################################################################

# The box is gone or wedged. Every hosted app is down; nothing else will tell
# you.
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
  treat_missing_data  = "breaching"
  alarm_description   = "The runner box is failing its status checks. Every hosted app is down."
  alarm_actions       = [aws_sns_topic.alerts.arn]
  ok_actions          = [aws_sns_topic.alerts.arn]
  tags                = local.runner_tags
}

# A full disk takes out all ten apps and Postgres at once, is caused by ordinary
# use, and is the outage this box will actually have. Needs the CloudWatch agent
# installed to report; without it this alarm sits in INSUFFICIENT_DATA, which is
# itself worth noticing.
resource "aws_cloudwatch_metric_alarm" "runner_disk" {
  alarm_name          = "boxcode-runner-disk"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "disk_used_percent"
  namespace           = "CWAgent"
  period              = 300
  statistic           = "Maximum"
  threshold           = 80
  alarm_description   = "Runner disk above 80%. A full disk takes every hosted app down together."
  alarm_actions       = [aws_sns_topic.alerts.arn]
  tags                = local.runner_tags
}

output "runner_instance_id" {
  value = aws_instance.runner.id
}

output "runner_public_ip" {
  description = "Point apps.boxcode.sh at this, and set it as the CloudFront origin for /api/*."
  value       = aws_eip.runner.public_ip
}
