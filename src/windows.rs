use crate::{
    metrics::{Metric, id},
    platform::probe,
};
use serde::Deserialize;
#[derive(Deserialize)]
struct Counter {
    kind: String,
    name: String,
    value: f64,
}
pub fn sensors() -> Vec<Metric> {
    // Formatted WMI engine counters are per process. Combine instances of one physical engine.
    let script = r#"
$ErrorActionPreference='SilentlyContinue'
$list=@()
$engines=Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine
foreach($group in ($engines | Group-Object { $_.Name -replace '^pid_\d+_', '' })){
$list+=@{kind='gpu';name=$group.Name;value=[double](($group.Group|Measure-Object UtilizationPercentage -Sum).Sum)}
}
foreach($meter in Get-CimInstance Win32_PerfFormattedData_Counters_PowerMeter){
if($meter.Name -ne '_Total'){$list+=@{kind='meter';name=$meter.Name;value=[double]$meter.Power}}
}
foreach($b in Get-CimInstance -Namespace root/wmi BatteryStatus){
if($b.Discharging -and $b.DischargeRate -gt 0 -and $b.DischargeRate -lt 4294967295){$list+=@{kind='battery';name=$b.InstanceName;value=[double]$b.DischargeRate/1000}}
}
foreach($z in Get-CimInstance -Namespace root/wmi MSAcpi_ThermalZoneTemperature){
if($z.CurrentTemperature -gt 0){$list+=@{kind='temperature';name=$z.InstanceName;value=[double]$z.CurrentTemperature/10-273.15}}
}
ConvertTo-Json -InputObject @($list) -Compress
"#;
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let exe = format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    let mut m = Vec::new();
    if let Some(bytes) = probe(&exe, &["-NoProfile", "-NonInteractive", "-Command", script]) {
        for row in serde_json::from_slice::<Vec<Counter>>(&bytes).unwrap_or_default() {
            let (group, label, unit, scope) = match row.kind.as_str() {
                "gpu" => (
                    "gpu",
                    row.name.rsplit("engtype_").next().unwrap_or("GPU engine"),
                    "%",
                    "host GPU",
                ),
                "battery" => ("power", "Battery discharge", "W", "battery"),
                "meter" => ("power", row.name.as_str(), "W", "host"),
                "temperature" => ("temperature", row.name.as_str(), "°C", "host"),
                _ => continue,
            };
            let v = if row.kind == "gpu" {
                row.value.clamp(0., 100.)
            } else {
                row.value
            };
            m.push(Metric::new(
                format!("{group}.{}.{}", row.kind, id(&row.name)),
                label,
                group,
                unit,
                v,
                scope,
            ));
        }
    }
    m
}
