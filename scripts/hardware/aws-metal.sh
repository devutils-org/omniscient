#!/bin/sh
# A bare-metal EC2 instance for the hardware cells, by the hour: started,
# swept through scripts/hardware/matrix.sh, and terminated -- whatever
# happens to the run.
#
#   scripts/hardware/aws-metal.sh plan             # what `run` would start, and nothing else
#   scripts/hardware/aws-metal.sh run --yes        # up, matrix.sh, down
#   scripts/hardware/aws-metal.sh up --yes         # start one; target/hardware/hosts names it
#   scripts/hardware/aws-metal.sh down             # terminate it, and what was made for it
#   scripts/hardware/aws-metal.sh sweep-orphans [--terminate]
#
# Settings, from the environment:
#   OMN_AWS_TYPE      m7i.metal-24xl (Sapphire Rapids: PT, topdown, PEBS,
#                     uncore, resctrl/MBM); c7a.metal-48xl for an AMD Genoa
#   OMN_AWS_REGION    the AWS CLI's own default otherwise
#   OMN_AWS_SPOT      1 for a spot instance (cheaper, may be reclaimed)
#   OMN_AWS_HOURS     2: the instance powers itself off -- and is terminated --
#                     after this many hours, whatever happens here
#   OMN_AWS_PRICE     the hourly price, for the estimate (the script does not
#                     look it up: https://aws.amazon.com/ec2/pricing/)
#   DRY_RUN=1         print every AWS call instead of making it
#
# Nothing that costs money is started without --yes. Three nets against an
# instance left running: `run` terminates it on exit (a trap); the instance
# shuts itself down after OMN_AWS_HOURS with shutdown behaviour `terminate`;
# and every resource is tagged `omn-hardware`, which `sweep-orphans` lists.
# The credentials are the AWS CLI's (`aws configure`, or AWS_PROFILE); this
# script never reads, writes or prints them.
set -eu
cd "$(dirname "$0")/../.."
state_dir=target/hardware
state=$state_dir/aws.state
type=${OMN_AWS_TYPE:-m7i.metal-24xl}
hours=${OMN_AWS_HOURS:-2}
spot=${OMN_AWS_SPOT:-0}
region_flag=""
[ -n "${OMN_AWS_REGION:-}" ] && region_flag="--region $OMN_AWS_REGION"
# Ubuntu 24.04: a kernel with BTF, cloud-init, and an `ubuntu` login with
# passwordless sudo, which distroproof's agent needs.
ami_parameter=/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id
case $type in
    *g.metal*|*gd.metal*|*gn.metal*) ami_parameter=$(echo "$ami_parameter" | sed 's/amd64/arm64/') ;;
esac

say() { printf 'aws-metal: %s\n' "$*" >&2; }

aws_() {
    if [ "${DRY_RUN:-0}" = 1 ]; then
        say "would run: aws $region_flag $*"
        case "$*" in
            *get-parameter*) echo ami-dryrun ;;
            *create-security-group*) echo sg-dryrun ;;
            *run-instances*) echo i-dryrun ;;
            *describe-instances*PublicIpAddress*) echo 192.0.2.10 ;;
            *describe-vpcs*) echo vpc-dryrun ;;
        esac
        return 0
    fi
    # shellcheck disable=SC2086
    aws $region_flag --output text "$@"
}

need_cli() {
    [ "${DRY_RUN:-0}" = 1 ] && return 0
    command -v aws >/dev/null 2>&1 || {
        say "the AWS CLI is not installed (https://aws.amazon.com/cli/)"
        exit 2
    }
    aws $region_flag sts get-caller-identity >/dev/null 2>&1 || {
        say "the AWS CLI has no usable credentials: \`aws configure\`, or AWS_PROFILE"
        exit 2
    }
}

plan() {
    say "instance: $type, $( [ "$spot" = 1 ] && echo spot || echo on-demand ), \
region ${OMN_AWS_REGION:-the CLI's default}"
    say "image: Ubuntu 24.04 ($ami_parameter)"
    say "it terminates itself after ${hours} h at the latest"
    if [ -n "${OMN_AWS_PRICE:-}" ]; then
        say "at most $(echo "$OMN_AWS_PRICE * $hours" | bc -l | xargs printf '%.2f') USD \
($OMN_AWS_PRICE/h for ${hours} h)"
    else
        say "price: set OMN_AWS_PRICE to the hourly price for an estimate \
(https://aws.amazon.com/ec2/pricing/)"
    fi
}

confirmed() {
    for arg in "$@"; do [ "$arg" = --yes ] && return 0; done
    plan
    say "nothing started: this costs money, pass --yes to start it"
    exit 3
}

up() {
    need_cli
    if [ -f "$state" ]; then
        say "an instance is already recorded in $state: \`$0 down\` first"
        exit 2
    fi
    mkdir -p "$state_dir"
    stamp=omn-hardware-$(date +%Y%m%d-%H%M%S)
    tag="ResourceType=%s,Tags=[{Key=omn-hardware,Value=$stamp}]"
    plan
    # A key for this instance alone, never reused.
    key=$state_dir/$stamp
    ssh-keygen -q -t ed25519 -N '' -C "$stamp" -f "$key"
    {
        echo "stamp=$stamp"
        echo "region=${OMN_AWS_REGION:-}"
        echo "started=$(date +%s)"
        echo "key=$key"
    } >"$state"
    aws_ ec2 import-key-pair --key-name "$stamp" \
        --public-key-material "fileb://$key.pub" \
        --tag-specifications "$(printf "$tag" key-pair)" >/dev/null
    echo "key_pair=$stamp" >>"$state"
    # SSH from this machine's address only.
    me=$(curl -fsS https://checkip.amazonaws.com | tr -d '[:space:]')
    vpc=$(aws_ ec2 describe-vpcs --filters Name=is-default,Values=true \
        --query 'Vpcs[0].VpcId')
    sg=$(aws_ ec2 create-security-group --group-name "$stamp" --vpc-id "$vpc" \
        --description "omn hardware cell, ssh from $me" \
        --tag-specifications "$(printf "$tag" security-group)" --query GroupId)
    echo "security_group=$sg" >>"$state"
    aws_ ec2 authorize-security-group-ingress --group-id "$sg" \
        --protocol tcp --port 22 --cidr "$me/32" >/dev/null
    ami=$(aws_ ssm get-parameter --name "$ami_parameter" --query Parameter.Value)
    market=""
    [ "$spot" = 1 ] && market="--instance-market-options MarketType=spot"
    user_data=$(printf '#!/bin/sh\nshutdown -h +%s\n' "$((hours * 60))")
    # shellcheck disable=SC2086
    instance=$(aws_ ec2 run-instances --image-id "$ami" --instance-type "$type" \
        --key-name "$stamp" --security-group-ids "$sg" $market \
        --instance-initiated-shutdown-behavior terminate \
        --user-data "$user_data" \
        --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=40,DeleteOnTermination=true}' \
        --tag-specifications "$(printf "$tag" instance)" \
        --query 'Instances[0].InstanceId')
    echo "instance=$instance" >>"$state"
    say "started $instance; waiting for it to run"
    aws_ ec2 wait instance-running --instance-ids "$instance"
    ip=$(aws_ ec2 describe-instances --instance-ids "$instance" \
        --query 'Reservations[0].Instances[0].PublicIpAddress')
    echo "ip=$ip" >>"$state"
    host_name=$(echo "$type" | tr . -)
    printf '%s ubuntu@%s 22 %s\n' "$host_name" "$ip" "$PWD/$key" >"$state_dir/hosts"
    if [ "${DRY_RUN:-0}" != 1 ]; then
        say "waiting for ssh on $ip (a metal instance takes minutes to boot)"
        i=0
        until ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
            -o ConnectTimeout=10 -i "$key" "ubuntu@$ip" true 2>/dev/null; do
            i=$((i + 1))
            if [ $i -gt 90 ]; then
                say "no ssh after 15 min"
                return 1
            fi
            sleep 10
        done
    fi
    say "ready: $state_dir/hosts names ubuntu@$ip"
}

down() {
    if [ ! -f "$state" ]; then
        say "no instance recorded in $state"
        return 0
    fi
    need_cli
    # shellcheck disable=SC1090
    . "./$state"
    if [ -n "${region:-}" ] && [ -z "$region_flag" ]; then region_flag="--region $region"; fi
    if [ -n "${instance:-}" ]; then
        aws_ ec2 terminate-instances --instance-ids "$instance" >/dev/null
        say "terminating $instance"
        aws_ ec2 wait instance-terminated --instance-ids "$instance"
    fi
    # The group can only go once nothing uses it.
    [ -n "${security_group:-}" ] && aws_ ec2 delete-security-group --group-id "$security_group"
    [ -n "${key_pair:-}" ] && aws_ ec2 delete-key-pair --key-name "$key_pair"
    [ -n "${key:-}" ] && rm -f "$key" "$key.pub"
    if [ -n "${started:-}" ]; then
        elapsed=$(( $(date +%s) - started ))
        hours_used=$(echo "$elapsed / 3600" | bc -l)
        line="ran $(printf '%.2f' "$hours_used") h"
        if [ -n "${OMN_AWS_PRICE:-}" ]; then
            line="$line, about $(echo "$hours_used * $OMN_AWS_PRICE" | bc -l | xargs printf '%.2f') USD"
        fi
        say "$line"
    fi
    rm -f "$state" "$state_dir/hosts"
}

sweep_orphans() {
    need_cli
    found=$(aws_ ec2 describe-instances \
        --filters Name=tag-key,Values=omn-hardware \
        Name=instance-state-name,Values=pending,running,stopping,stopped \
        --query 'Reservations[].Instances[].[InstanceId,InstanceType,LaunchTime]')
    if [ -z "$found" ]; then
        say "no instance tagged omn-hardware"
        return 0
    fi
    echo "$found"
    for arg in "$@"; do
        if [ "$arg" = --terminate ]; then
            # shellcheck disable=SC2046
            aws_ ec2 terminate-instances --instance-ids $(echo "$found" | cut -f1) >/dev/null
            say "terminating them"
            return 0
        fi
    done
    say "pass --terminate to terminate them"
}

command=${1:-}
[ $# -gt 0 ] && shift
case $command in
    plan) plan ;;
    up) confirmed "$@"; up ;;
    down) down ;;
    run)
        confirmed "$@"
        trap 'down' EXIT INT TERM
        up
        scripts/hardware/matrix.sh
        ;;
    sweep-orphans) sweep_orphans "$@" ;;
    *)
        sed -n '2,/^set -eu$/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2
        exit 2
        ;;
esac
