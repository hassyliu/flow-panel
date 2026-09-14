package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"net"
	"net/url"
	"strconv"
	"strings"

	"golang.org/x/net/idna"
)

const maxWorkers = 64
const maxURLs = 500
const chunkSize = 64 * 1024

type Config struct {
	URLs        []string `json:"urls"`
	Concurrency int      `json:"concurrency"`
	Repeat      int      `json:"repeat"`
	Interval    float64  `json:"interval"`
	Timeout     float64  `json:"timeout"`
	RateMiB     float64  `json:"rate_mib"`
	MaxGiB      float64  `json:"max_gib"`
	Duration    float64  `json:"duration"`
	ServerIPs   []string `json:"server_ips"`
}

func hostKey(host string) (string, error) {
	if net.ParseIP(host) != nil {
		return strings.ToLower(host), nil
	}
	return idna.Lookup.ToASCII(strings.ToLower(strings.TrimSuffix(host, ".")))
}

func normalizeURL(raw string) (string, error) {
	raw = strings.TrimSpace(raw)
	if len(raw) > 8192 {
		return "", errors.New("下载链接过长")
	}
	for _, c := range raw {
		if c < 32 || c == 127 {
			return "", errors.New("下载链接包含控制字符")
		}
	}
	u, err := url.Parse(raw)
	if err != nil || u == nil || (u.Scheme != "http" && u.Scheme != "https") || u.Hostname() == "" || u.User != nil {
		return "", errors.New("请填写完整的 HTTP / HTTPS 链接，不包含账号密码")
	}
	host, err := hostKey(u.Hostname())
	if err != nil {
		return "", errors.New("域名格式错误")
	}
	if port := u.Port(); port != "" {
		n, e := strconv.Atoi(port)
		if e != nil || n < 1 || n > 65535 {
			return "", errors.New("端口范围为 1–65535")
		}
		u.Host = net.JoinHostPort(host, port)
	} else if strings.Contains(host, ":") {
		u.Host = "[" + host + "]"
	} else {
		u.Host = host
	}
	if u.Path == "" {
		u.Path = "/"
	}
	u.Fragment = ""
	return u.String(), nil
}

func parseServerIPs(raw json.RawMessage) ([]string, error) {
	result := []string{}
	if len(raw) == 0 || string(raw) == "null" {
		return result, nil
	}
	var lines []string
	var value string
	if json.Unmarshal(raw, &value) == nil {
		lines = strings.Split(value, "\n")
	} else if err := json.Unmarshal(raw, &lines); err != nil {
		return nil, errors.New("服务器 IP 必须是每行一个 IP 的文本或字符串数组")
	}
	seen := map[string]bool{}
	for i, line := range lines {
		ipText := strings.TrimSpace(line)
		if ipText == "" {
			continue
		}
		if strings.HasPrefix(ipText, "[") && strings.HasSuffix(ipText, "]") {
			ipText = ipText[1 : len(ipText)-1]
		}
		ip := net.ParseIP(ipText)
		if ip == nil {
			return nil, fmt.Errorf("第 %d 行 IP 格式错误：每行只填一个 IPv4 / IPv6 地址，不含域名或端口", i+1)
		}
		if !seen[ip.String()] {
			seen[ip.String()] = true
			result = append(result, ip.String())
		}
		if len(result) > maxURLs {
			return nil, errors.New("每条任务最多 500 个服务器 IP")
		}
	}
	return result, nil
}

func parseConfig(body []byte) (Config, error) {
	c := Config{Concurrency: 8, Repeat: 1, Interval: .2, Timeout: 15}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(body, &fields); err != nil || fields == nil {
		return c, errors.New("请求内容必须为 JSON 对象")
	}
	if b, ok := fields["urls"]; ok {
		var text string
		if json.Unmarshal(b, &text) == nil {
			for _, line := range strings.Split(text, "\n") {
				if line = strings.TrimSpace(line); line != "" {
					c.URLs = append(c.URLs, line)
				}
			}
		} else if err := json.Unmarshal(b, &c.URLs); err != nil {
			return c, errors.New("下载链接格式错误")
		}
	}
	if len(c.URLs) < 1 || len(c.URLs) > maxURLs {
		return c, errors.New("请填写 1–500 条下载链接")
	}
	urls := []string{}
	seen := map[string]bool{}
	for _, raw := range c.URLs {
		u, err := normalizeURL(raw)
		if err != nil {
			return c, err
		}
		if !seen[u] {
			seen[u] = true
			urls = append(urls, u)
		}
	}
	c.URLs = urls
	specs := []struct {
		key           string
		min, max, def float64
		integral      bool
		set           func(float64)
	}{
		{"concurrency", 1, maxWorkers, 8, true, func(v float64) { c.Concurrency = int(v) }},
		{"repeat", 0, 1000000, 1, true, func(v float64) { c.Repeat = int(v) }},
		{"interval", 0, 3600, .2, false, func(v float64) { c.Interval = v }},
		{"timeout", 1, 120, 15, false, func(v float64) { c.Timeout = v }},
		{"rate_mib", 0, 100000, 0, false, func(v float64) { c.RateMiB = v }},
		{"max_gib", 0, 1000000, 0, false, func(v float64) { c.MaxGiB = v }},
		{"duration", 0, 31536000, 0, false, func(v float64) { c.Duration = v }},
	}
	for _, spec := range specs {
		v := spec.def
		if raw, ok := fields[spec.key]; ok {
			if string(raw) == "null" || json.Unmarshal(raw, &v) != nil {
				return c, fmt.Errorf("%s 必须为数字", spec.key)
			}
		}
		if math.IsNaN(v) || math.IsInf(v, 0) || v < spec.min || v > spec.max || (spec.integral && math.Trunc(v) != v) {
			return c, fmt.Errorf("%s 取值范围 %g–%g", spec.key, spec.min, spec.max)
		}
		spec.set(v)
	}
	var err error
	if _, old := fields["host_overrides"]; old {
		return c, errors.New("请刷新页面，使用 server_ips 填写 IP 列表，每行一个 IP")
	}
	c.ServerIPs, err = parseServerIPs(fields["server_ips"])
	return c, err
}
